// 助理的记忆页（docs/ASSISTANT.md「记忆」，设计稿根目录 docs/ASSISTANT_MEMORY_REDO_DESIGN_2026-11-26.md）：
// 一页一个 Markdown，分“关于你 / 项目 / 别的”；你写的行标出来（助理和整理改不了，只能问你）；每晚整理的结果和撤销；
// 搜记忆和以前的对话（带前后文）；每页的历史、撤销、回到某一版；删掉的 30 天内拿回来；彻底忘掉；同步/整理/索引/导出设置。
import { AlertTriangle, ChevronLeft, ChevronRight, Download, Moon, MoreHorizontal, Pencil, Plus, RefreshCw, Search, Undo2 } from 'lucide-react';
import { useCallback, useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';

import { api, fetchRaw } from '../shared/api.js';
import { errorText } from '../shared/i18n.js';
import Select from '../shared/Select.jsx';
import { baseName } from '../shared/sessions.jsx';
import { relativeTime } from '../shared/time.js';
import { Button, Switch, useConfirm, useToast } from '../shared/ui.jsx';

// 三组（assistant_memory.rs 的 Dir），页面上按这个顺序
const DIRS = ['you', 'projects', 'notes'];
// 搜到的哪一类（assistant_index.rs 的 src）
const SRC_ORDER = ['memory', 'journal', 'assistant', 'claude', 'codex', 'agy', 'imported'];
// 最近一次整理在列表顶上显示多久
const RUN_CARD_MS = 3 * 86_400_000;
// 历史里的改动最多画几行
const DIFF_LINES = 14;

// 记录的时间都是过去的；刚发生的（差 0 秒）别显示成“0 秒后”
const ago = (t, at) => relativeTime(t, Math.min(Number(at) || 0, Date.now() - 1000) || 0);

const dirOf = (id) => id.split('/')[0];
const stemOf = (id) => id.split('/').slice(1).join('/');

/** 一页去掉开头 `---` 块后的行：[{ n: 在整页里第几行（从 1 起）, text }]，前后的空行不要 */
function bodyLines(text) {
  const all = String(text || '').split('\n');
  let from = 0;
  if (all[0] === '---') {
    const end = all.indexOf('---', 1);
    if (end > 0) from = end + 1;
  }
  const out = all.slice(from).map((t, i) => ({ n: from + i + 1, text: t }));
  while (out.length && !out[0].text.trim()) out.shift();
  while (out.length && !out[out.length - 1].text.trim()) out.pop();
  return out;
}

const bodyText = (text) =>
  bodyLines(text)
    .map((l) => l.text)
    .join('\n');

/** 两段正文的差别：去掉的行、加上的行（按行比，重复的行按次数算） */
function lineDiff(before, after) {
  const a = before == null ? [] : bodyText(before).split('\n').filter((l) => l.trim());
  const b = after == null ? [] : bodyText(after).split('\n').filter((l) => l.trim());
  const left = new Map();
  for (const l of b) left.set(l, (left.get(l) || 0) + 1);
  const del = [];
  for (const l of a) {
    if (left.get(l)) left.set(l, left.get(l) - 1);
    else del.push(['del', l]);
  }
  const keep = new Map();
  for (const l of a) keep.set(l, (keep.get(l) || 0) + 1);
  const add = [];
  for (const l of b) {
    if (keep.get(l)) keep.set(l, keep.get(l) - 1);
    else add.push(['add', l]);
  }
  return [...del, ...add];
}

/** 开头 `---` 里的说明、叫法（显示改了说明、叫法时用） */
function frontOf(text) {
  const all = String(text || '').split('\n');
  const end = all[0] === '---' ? all.indexOf('---', 1) : -1;
  const head = end > 0 ? all.slice(1, end) : [];
  const val = (k) => {
    const l = head.find((x) => x.trim().startsWith(`${k}:`));
    if (!l) return '';
    const v = l.slice(l.indexOf(':') + 1).trim();
    try {
      return v.startsWith('"') ? JSON.parse(v) : v;
    } catch {
      return v;
    }
  };
  return { description: val('description'), aliases: val('aliases') };
}

function Diff({ rows }) {
  if (!rows.length) return null;
  const shown = rows.slice(0, DIFF_LINES);
  return (
    <div className="mem-diff">
      {shown.map(([k, l], i) => (
        <div key={i} className={k}>
          {l.replace(/^\s*[-*] |^#+ /, '')}
        </div>
      ))}
      {rows.length > shown.length && <div className="more">…</div>}
    </div>
  );
}

/** 一行正文：`## ` 小标题、`- ` 列表，别的照原样 */
function Line({ text }) {
  if (text.startsWith('## ') || text.startsWith('### ')) return <h5>{text.replace(/^#+ /, '')}</h5>;
  if (/^\s*[-*] /.test(text)) return <span className="li">{text.replace(/^\s*[-*] /, '')}</span>;
  if (!text.trim()) return <span className="gap" />;
  return text;
}

const splitAliases = (s) =>
  s
    .split(/[,，、;；]/)
    .map((x) => x.trim())
    .filter(Boolean);

/**
 * 记忆：助理记着的关于你、你的项目的事。“关于你”每次整页带上，别的页只带说明那一行，用到时助理再打开。
 * 每晚对着以前的对话整理一次，改动直接生效，能整次撤销；动到你写的行要你点头。
 */
export default function MemoryTab() {
  const { t } = useTranslation();
  const toast = useToast();
  const [data, setData] = useState(null);
  // { k: 'list' } | { k: 'page', id, pend? } | { k: 'new' } | { k: 'run', id }
  const [view, setView] = useState({ k: 'list' });
  const load = useCallback(() => api.get('/assistant/memory').then(setData, (e) => toast(errorText(t, e), 'error')), [t, toast]);
  useEffect(() => {
    load();
  }, [load]);
  const go = (v) => {
    setView(v);
    // 换页回到顶上（右栏自己滚）
    requestAnimationFrame(() => document.querySelector('.mem')?.closest('.asst-side-body')?.scrollTo({ top: 0 }));
  };
  const back = () => (go({ k: 'list' }), load());
  if (!data) return <div className="mem" aria-busy="true" />;
  const wide = view.k !== 'list' ? '' : undefined;
  return (
    <section className="mem" data-wide={wide}>
      {view.k === 'list' && <ListView data={data} go={go} reload={load} />}
      {view.k === 'page' && <PageView key={view.id} id={view.id} pend={view.pend} data={data} go={go} onBack={back} reload={load} />}
      {view.k === 'new' && <NewView go={go} onBack={back} reload={load} />}
      {view.k === 'run' && <RunView key={view.id} id={view.id} data={data} go={go} onBack={back} reload={load} />}
    </section>
  );
}

function Head({ title, onBack, children }) {
  const { t } = useTranslation();
  return (
    <div className="mem-ph">
      <button type="button" className="btn btn-plain btn-icon" onClick={onBack} aria-label={t('asst.mem.back')} title={t('asst.mem.back')}>
        <ChevronLeft size={18} aria-hidden="true" />
      </button>
      <h3>{title}</h3>
      {children}
    </div>
  );
}

// ---------------- 列表 ----------------

function ListView({ data, go, reload }) {
  const { t } = useTranslation();
  const [q, setQ] = useState('');
  const [hits, setHits] = useState(null);
  const needle = q.trim().toLowerCase();
  const match = (p) => !needle || `${p.name}\n${p.description}\n${(p.aliases || []).join(' ')}`.toLowerCase().includes(needle);
  const pending = data.pending.filter((p) => !p.stale || data.pages.some((x) => x.id === p.page));
  const r = data.resident;
  const runs = data.runs || [];
  const last = runs.find((x) => x.status !== 'skipped' || x.manual);
  const showCard = last && Date.now() - last.at < RUN_CARD_MS && (last.changes?.length || last.held?.length || last.pending?.length || last.undone_at);
  return (
    <>
      <p className="muted asst-small">{t('asst.mem.hint')}</p>
      <Finder q={q} setQ={setQ} hits={hits} setHits={setHits} go={go} reload={reload} />
      {!hits && (
        <>
          {!needle && showCard && <RunCard run={last} go={go} reload={reload} />}
          {!needle && pending.length > 0 && (
            <button type="button" className="mem-flag" onClick={() => go({ k: 'page', id: pending[0].page, pend: true })}>
              <AlertTriangle size={14} aria-hidden="true" />
              <span className="ellipsis">
                {t('asst.mem.pending_n', { count: pending.length })}
                {' · '}
                <span className="mem-flag-names">{[...new Set(pending.map((p) => stemOf(p.page)))].join('、')}</span>
              </span>
              <ChevronRight size={14} aria-hidden="true" />
            </button>
          )}
          {needle && <p className="muted asst-small">{t('asst.mem.filter_n', { count: data.pages.filter(match).length })}</p>}
          {data.pages.length === 0 && !needle && <p className="muted asst-small asst-empty">{t('asst.mem.empty')}</p>}
          {DIRS.map((d) => {
            const list = data.pages.filter((p) => p.dir === d).filter(match);
            if (!list.length) return null;
            return (
              <div key={d} className="mem-group">
                <h3 className="asst-h asst-h-sm">
                  {t(`asst.mem.dir_name.${d}`)}
                  <span className="asst-count-soft">{list.length}</span>
                  {d === 'you' && !needle && (
                    <span className="mem-budget" title={t('asst.mem.budget_tip')}>
                      {t('asst.mem.budget', { used: r.you_used.toLocaleString(), budget: r.you_budget.toLocaleString() })}
                    </span>
                  )}
                </h3>
                <ul className="mem-list">
                  {list.map((p) => (
                    <li key={p.id}>
                      <PageRow p={p} fresh={last && !last.undone_at && last.changes?.length > 0 && p.by === 'dream' && p.updated >= last.at} onOpen={() => go({ k: 'page', id: p.id })} />
                    </li>
                  ))}
                </ul>
              </div>
            );
          })}
          {r.you_left_out > 0 && !needle && <p className="muted asst-small">{t('asst.mem.you_left_out', { count: r.you_left_out })}</p>}
          {!needle && (
            <>
              <div className="mem-acts-row">
                <Button variant="secondary" className="btn-sm" onClick={() => go({ k: 'new' })}>
                  <Plus size={14} aria-hidden="true" />
                  {t('asst.mem.new_page')}
                </Button>
              </div>
              {runs.length > 0 && (
                <details className="mem-more">
                  <summary>{t('asst.mem.runs')}</summary>
                  <ul className="mem-list">
                    {runs.map((x) => (
                      <li key={x.id}>
                        <button type="button" className="mem-run" data-st={x.undone_at ? 'undone' : x.status} disabled={x.status === 'skipped'} onClick={() => go({ k: 'run', id: x.id })}>
                          <span className="mem-run-what">
                            <span>
                              <b>{runTitle(t, x)}</b>
                              <span className="muted"> · {ago(t, x.at)}</span>
                            </span>
                            {x.summary && <span className="muted">{x.summary}</span>}
                          </span>
                        </button>
                      </li>
                    ))}
                  </ul>
                </details>
              )}
              {data.deleted.length > 0 && <Deleted list={data.deleted} reload={reload} />}
              <MemorySettings data={data} reload={reload} go={go} />
            </>
          )}
        </>
      )}
    </>
  );
}

function PageRow({ p, fresh, onOpen }) {
  const { t } = useTranslation();
  return (
    <button type="button" className="mem-pg" onClick={onOpen}>
      <span className="mem-pg-main">
        <span className="mem-pg-name">
          {p.name}
          {p.aliases?.length > 0 && <span className="mem-pg-alias">{p.aliases.join(' · ')}</span>}
          {p.pending && <span className="mem-tag mem-tag-warn">{t('asst.mem.tag_pending')}</span>}
          {fresh && <span className="mem-tag mem-tag-new">{t('asst.mem.tag_fresh')}</span>}
          {p.user_lines > 0 && <span className="mem-tag">{t('asst.mem.tag_yours')}</span>}
        </span>
        {p.description && <span className="mem-pg-desc">{p.description}</span>}
      </span>
      <ChevronRight size={14} aria-hidden="true" />
    </button>
  );
}

/** 一次整理的一句话标题 */
function runTitle(t, r) {
  if (r.undone_at) return t('asst.mem.run_undone');
  if (r.status === 'skipped') return t('asst.mem.run_skipped');
  if (r.status === 'failed') return t('asst.mem.run_failed');
  if (r.status === 'held') return t(r.applied_at ? 'asst.mem.run_applied' : 'asst.mem.run_held');
  if (!r.changes?.length && !r.pending?.length) return t('asst.mem.run_none');
  return t('asst.mem.run_changed', { count: r.changes?.length || 0 });
}

/** 一次整理的小字：几条没生效、几处等确认、拦下的原因 */
function runSub(t, r) {
  const parts = [];
  if (r.status === 'held' && !r.applied_at) parts.push(t('asst.mem.run_held_why', { pct: Math.round((r.loss || 0) * 100) }));
  if (r.dropped?.length) parts.push(t('asst.mem.run_dropped', { count: r.dropped.length }));
  if (r.pending?.length) parts.push(t('asst.mem.run_pending', { count: r.pending.length }));
  if (r.status === 'failed' && r.error) parts.push(r.error);
  return parts.join(' · ');
}

/** 列表顶上：最近一次整理做了什么，看改动 / 撤销这次 */
function RunCard({ run, go, reload }) {
  const { t } = useTranslation();
  const undo = useRunUndo(run, reload);
  return (
    <div className="mem-dream" data-undone={run.undone_at ? '' : undefined}>
      <div className="mem-dream-h">
        <Moon size={14} aria-hidden="true" />
        <span>
          {ago(t, run.at)} · {runTitle(t, run)}
        </span>
      </div>
      {runSub(t, run) && <span className="muted asst-small">{runSub(t, run)}</span>}
      {run.summary && <p>{run.summary}</p>}
      <div className="mem-acts-row">
        <Button variant="secondary" className="btn-sm" onClick={() => go({ k: 'run', id: run.id })}>
          {t('asst.mem.run_open')}
        </Button>
        {undo.can && (
          <Button variant="plain" className="btn-sm" busy={undo.busy} onClick={undo.run}>
            <Undo2 size={14} aria-hidden="true" />
            {t('asst.mem.run_undo')}
          </Button>
        )}
      </div>
    </div>
  );
}

function useRunUndo(run, reload) {
  const { t } = useTranslation();
  const toast = useToast();
  const confirm = useConfirm();
  const [busy, setBusy] = useState(false);
  const can = run.status === 'ok' && !run.undone_at && run.changes?.length > 0;
  const go = async () => {
    const ok = await confirm({ title: t('asst.mem.run_undo_q'), body: t('asst.mem.run_undo_body', { count: run.changes.length }), confirm: t('asst.mem.run_undo') });
    if (!ok) return;
    setBusy(true);
    try {
      const r = await api.post(`/assistant/memory/runs/${run.id}/undo`);
      toast(r.skipped?.length ? t('asst.mem.run_undo_skipped', { pages: r.skipped.map(stemOf).join('、') }) : t('asst.mem.run_undo_done'));
    } catch (e) {
      toast(errorText(t, e), 'error');
    }
    setBusy(false);
    reload();
  };
  return { can, busy, run: go };
}

function Deleted({ list, reload }) {
  const { t } = useTranslation();
  const toast = useToast();
  const restore = (d) => api.post(`/assistant/memory/changes/${d.change}/undo`).then(reload, (e) => toast(errorText(t, e), 'error'));
  return (
    <details className="mem-more">
      <summary>{t('asst.mem.deleted', { count: list.length })}</summary>
      <ul className="mem-list">
        {list.map((d) => (
          <li key={d.change} className="mem-pg mem-pg-static">
            <span className="mem-pg-main">
              <span className="mem-pg-name">{stemOf(d.page)}</span>
              <span className="mem-pg-desc">
                {ago(t, d.at)} · {t(`asst.mem.by.${d.by}`, { defaultValue: d.by })}
                {d.description && ` · ${d.description}`}
              </span>
            </span>
            <Button variant="plain" className="btn-sm" onClick={() => restore(d)}>
              {t('asst.mem.restore')}
            </Button>
          </li>
        ))}
      </ul>
      <p className="muted asst-small">{t('asst.mem.deleted_hint')}</p>
    </details>
  );
}

// ---------------- 搜 ----------------

/** 打字先筛页；回车再搜以前的一切（记忆、流水、对话、会话、导进来的记忆），每条能看前后文 */
function Finder({ q, setQ, hits, setHits, go, reload }) {
  const { t } = useTranslation();
  const toast = useToast();
  const confirm = useConfirm();
  const [busy, setBusy] = useState(false);
  const needle = q.trim();
  const search = (ev) => {
    ev?.preventDefault();
    if (!needle) return;
    setBusy(true);
    api.post('/assistant/memory-search', { q }).then(
      (r) => (setHits(r.hits), setBusy(false)),
      (e) => (toast(errorText(t, e), 'error'), setBusy(false)),
    );
  };
  const forget = async (h) => {
    const inPlace = ['memory', 'journal', 'assistant'].includes(h.src);
    const ok = await confirm({ title: t('asst.mem.forget_hit_q'), body: t(inPlace ? 'asst.mem.forget_hit_body' : 'asst.mem.forget_hit_body_index'), confirm: t('asst.mem.purge'), danger: true });
    if (!ok) return;
    try {
      const r = await api.post(`/assistant/memory-search/${h.h}/forget`);
      setHits((l) => l.filter((x) => x.h !== h.h));
      toast(t(r.source === 'removed' ? 'asst.mem.forgot_hit' : 'asst.mem.forgot_hit_index'));
      reload();
    } catch (e) {
      toast(errorText(t, e), 'error');
    }
  };
  // 认不得的来源排在最后，不吞掉
  const order = hits ? [...SRC_ORDER, ...new Set(hits.map((h) => h.src).filter((s) => !SRC_ORDER.includes(s)))] : [];
  const groups = order.map((s) => [s, hits.filter((h) => h.src === s)]).filter(([, l]) => l.length);
  return (
    <div className="mem-find">
      <form className="mem-search" onSubmit={search} role="search">
        <Search size={14} aria-hidden="true" />
        <input type="search" value={q} onChange={(e) => (setQ(e.target.value), setHits(null))} placeholder={t('asst.mem.filter')} aria-label={t('asst.mem.filter')} enterKeyHint="search" />
        {needle && (
          <Button variant="plain" className="btn-sm" type="submit" busy={busy}>
            {t('asst.mem.search_all')}
          </Button>
        )}
      </form>
      {hits && hits.length === 0 && <p className="muted asst-small">{t('asst.mem.search_none')}</p>}
      {groups.map(([src, list]) => (
        <div key={src} className="mem-hits">
          <h4 className="mem-hits-h">{t(`asst.mem.src.${src}`, { defaultValue: src })}</h4>
          <ul className="mem-list">
            {list.map((h) => (
              <Hit key={h.h} h={h} onOpen={src === 'memory' ? () => go({ k: 'page', id: h.ref }) : null} onForget={() => forget(h)} />
            ))}
          </ul>
        </div>
      ))}
      {hits && hits.length > 0 && <p className="muted asst-small">{t('asst.mem.search_note')}</p>}
      {hits && (
        <Button variant="plain" className="btn-sm mem-self-start" onClick={() => (setQ(''), setHits(null))}>
          {t('asst.mem.search_back')}
        </Button>
      )}
    </div>
  );
}

function hitMeta(t, h) {
  const role = h.role === 'user' || h.role === 'assistant' ? t(`asst.mem.role.${h.role}`) : '';
  return [h.src === 'memory' ? stemOf(h.ref) : h.title, h.workdir && baseName(h.workdir), role, h.at ? ago(t, h.at) : h.src === 'journal' ? h.ref : ''].filter(Boolean).join(' · ');
}

function Hit({ h, onOpen, onForget }) {
  const { t } = useTranslation();
  return (
    <li className="mem-hit">
      {onOpen ? (
        <button type="button" className="mem-hit-main" onClick={onOpen}>
          <span className="mem-hit-meta">{hitMeta(t, h)}</span>
          <span className="mem-hit-text">{h.snippet}</span>
        </button>
      ) : (
        <div className="mem-hit-main">
          <span className="mem-hit-meta">{hitMeta(t, h)}</span>
          <span className="mem-hit-text">{h.snippet}</span>
        </div>
      )}
      <div className="mem-hit-acts">
        {!onOpen && <Context loc={h.h} />}
        <button type="button" className="mem-lnk mem-lnk-danger" onClick={onForget}>
          {t('asst.mem.purge')}
        </button>
      </div>
    </li>
  );
}

/** 按位置展开前后文（搜到的一段、改动的依据） */
function Context({ loc, label }) {
  const { t } = useTranslation();
  const toast = useToast();
  const [src, setSrc] = useState(null);
  const [open, setOpen] = useState(false);
  if (!loc || loc.startsWith('mem:')) return null;
  const toggle = () => {
    if (open) return setOpen(false);
    setOpen(true);
    if (!src)
      api.get(`/assistant/memory-source?h=${encodeURIComponent(loc)}`).then(setSrc, (e) => {
        setOpen(false);
        toast(errorText(t, e), 'error');
      });
  };
  return (
    <>
      <button type="button" className="mem-lnk" onClick={toggle} aria-expanded={open}>
        {open ? t('asst.mem.ctx_hide') : label || t('asst.mem.ctx')}
      </button>
      {open && src && (
        <div className="mem-quote mem-quote-full">
          {src.pieces.map((p) => (
            <div key={p.h} className={p.hit ? 'hit' : 'ctx'}>
              {p.role === 'user' || p.role === 'assistant' ? `${t(`asst.mem.role.${p.role}`)}：` : ''}
              {p.body}
            </div>
          ))}
        </div>
      )}
    </>
  );
}

// ---------------- 依据 ----------------

function basisWhere(t, b) {
  return [t(`asst.mem.src.${b.src}`, { defaultValue: b.src || t('asst.mem.src.memory') }), b.at ? ago(t, b.at) : ''].filter(Boolean).join(' · ');
}

/** 改动的依据：点开看原话，原始记录里的还能看前后文 */
function Basis({ list }) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  if (!list?.length) return null;
  return (
    <>
      <button type="button" className="mem-lnk" onClick={() => setOpen(!open)} aria-expanded={open}>
        {open ? t('asst.mem.basis_hide') : t('asst.mem.basis')}
      </button>
      {open && (
        <div className="mem-basis">
          {list.map((b, i) => (
            <div key={i} className="mem-quote">
              <span className="ctx">
                {basisWhere(t, b)}
                {b.user && ` · ${t('asst.mem.basis_you')}`}
              </span>
              {b.text && <span className="hit">{b.text}</span>}
              <Context loc={b.loc} />
            </div>
          ))}
        </div>
      )}
    </>
  );
}

// ---------------- 一页 ----------------

function PageView({ id, pend, data, go, onBack, reload }) {
  const { t } = useTranslation();
  const toast = useToast();
  const confirm = useConfirm();
  const [p, setP] = useState(null);
  const [edit, setEdit] = useState(false);
  const [menu, setMenu] = useState(false);
  const [busy, setBusy] = useState('');
  const [note, setNote] = useState('');
  const load = useCallback(() => api.get(`/assistant/memory/page?id=${encodeURIComponent(id)}`).then(setP, (e) => toast(errorText(t, e), 'error')), [id, t, toast]);
  useEffect(() => {
    load();
  }, [load]);
  useEffect(() => {
    if (pend && p?.page?.proposal) document.getElementById('mem-pend')?.scrollIntoView({ block: 'center', behavior: 'smooth' });
  }, [pend, p]);
  const after = () => (load(), reload());
  const fail = (e) => toast(errorText(t, e), 'error');
  if (!p) return <Head title={stemOf(id)} onBack={onBack} />;
  const page = p.page;
  const hist = [...(p.history || [])].sort((a, b) => b.at - a.at);
  const dir = dirOf(id);
  if (!page)
    return (
      <>
        <Head title={stemOf(id)} onBack={onBack} />
        <p className="muted asst-small">{t('asst.mem.page_gone')}</p>
        <History hist={hist} id={id} after={after} />
      </>
    );
  if (edit) return <EditView page={page} onCancel={() => setEdit(false)} onSaved={(nid) => (setEdit(false), nid !== id ? (reload(), go({ k: 'page', id: nid })) : after())} />;
  const userSet = new Set(page.user);
  const lines = bodyLines(page.text);
  const prop = page.proposal;
  const del = async () => {
    setMenu(false);
    const ok = await confirm({ title: t('asst.mem.delete_q', { name: page.name }), body: t('asst.mem.delete_body'), confirm: t('asst.mem.delete'), danger: true });
    if (ok) api.post('/assistant/memory/page/delete', { id }).then(onBack, fail);
  };
  const purge = async () => {
    setMenu(false);
    const ok = await confirm({ title: t('asst.mem.purge_q', { name: page.name }), body: t(dir === 'you' ? 'asst.mem.purge_body_you' : 'asst.mem.purge_body'), confirm: t('asst.mem.purge'), danger: true });
    if (ok) api.post('/assistant/memory/page/purge', { id }).then(onBack, fail);
  };
  const decide = (action) => {
    setBusy(action);
    api.post(`/assistant/memory/pending/${prop.id}/${action}`).then(
      () => (setBusy(''), setNote(t(action === 'confirm' ? 'asst.mem.pend_done_ok' : 'asst.mem.pend_done_no')), after()),
      (e) => (setBusy(''), fail(e)),
    );
  };
  // 等确认的改动画在哪几行上
  const propDiff = prop ? lineDiff(page.text, prop.proposed) : [];
  const hl = new Set(propDiff.filter(([k]) => k === 'del').map(([, l]) => l));
  return (
    <>
      <Head title={page.name} onBack={onBack}>
        <Button variant="plain" className="btn-sm" onClick={() => setEdit(true)}>
          <Pencil size={14} aria-hidden="true" />
          {t('asst.mem.edit')}
        </Button>
        <button type="button" className="btn btn-plain btn-icon" onClick={() => setMenu(!menu)} aria-expanded={menu} aria-label={t('asst.mem.more')} title={t('asst.mem.more')}>
          <MoreHorizontal size={16} aria-hidden="true" />
        </button>
      </Head>
      {menu && (
        <div className="mem-menu" role="menu">
          <button type="button" role="menuitem" onClick={del}>
            {t('asst.mem.delete_item')}
          </button>
          <button type="button" role="menuitem" className="mem-danger" onClick={purge}>
            {t('asst.mem.purge_item')}
          </button>
        </div>
      )}
      <div className="mem-meta">
        <span className="mem-path">/memories/{id}.md</span>
        <span>
          {t(`asst.mem.dir_name.${dir}`)} · {t(dir === 'you' ? 'asst.mem.resident_you' : 'asst.mem.resident_other')}
          {page.path && ` · ${page.path}`}
        </span>
      </div>
      {page.aliases?.length > 0 && (
        <div className="mem-chips">
          <span className="muted asst-small">{t('asst.mem.aliases')}</span>
          {page.aliases.map((a) => (
            <span key={a} className="mem-chip">
              {a}
            </span>
          ))}
        </div>
      )}
      {page.description && <p className="mem-desc">{page.description}</p>}
      {prop && (
        <div className="mem-pend" id="mem-pend">
          <div className="mem-pend-h">
            <AlertTriangle size={14} aria-hidden="true" />
            {t(prop.proposed == null ? 'asst.mem.pend_delete' : 'asst.mem.pend_head', { who: t(`asst.mem.by.${prop.by}`, { defaultValue: prop.by }) })}
          </div>
          <Diff rows={propDiff} />
          {prop.why && <span className="muted asst-small">{t('asst.mem.because', { why: prop.why })}</span>}
          <div className="mem-acts-row">
            <Basis list={prop.basis} />
          </div>
          {prop.stale && <span className="muted asst-small">{t('asst.mem.pend_stale')}</span>}
          <div className="mem-acts-row">
            {!prop.stale && (
              <Button className="btn-sm" busy={busy === 'confirm'} disabled={Boolean(busy)} onClick={() => decide('confirm')}>
                {t('asst.mem.pend_ok')}
              </Button>
            )}
            <Button variant="plain" className="btn-sm" busy={busy === 'decline'} disabled={Boolean(busy)} onClick={() => decide('decline')}>
              {t('asst.mem.pend_no')}
            </Button>
          </div>
        </div>
      )}
      {note && <p className="muted asst-small">{note}</p>}
      <div className="mem-doc">
        {lines.length === 0 && <span className="muted asst-small">{t('asst.mem.page_empty')}</span>}
        {lines.map((l) => (
          <div key={l.n} className="mem-ln" data-by={userSet.has(l.n) ? 'user' : undefined} data-hl={hl.has(l.text) ? '' : undefined}>
            {userSet.has(l.n) && (
              <span className="mem-you" title={t('asst.mem.yours_tip')}>
                {t('asst.mem.you_mark')}
              </span>
            )}
            <Line text={l.text} />
          </div>
        ))}
      </div>
      {page.user_lines > 0 && (
        <div className="mem-legend">
          <i />
          {t('asst.mem.legend')}
        </div>
      )}
      <p className="muted asst-small">
        {t(`asst.mem.by.${page.by}`, { defaultValue: page.by })} · {ago(t, page.updated)}
        {page.used_at > 0 && ` · ${t('asst.mem.used', { when: ago(t, page.used_at) })}`}
      </p>
      <History hist={hist} id={id} after={after} runs={data.runs} go={go} />
    </>
  );
}

/** 一页的历史：每次改动谁改的、改了什么、依据；最近那次能撤销，以前的能回到那一版 */
function History({ hist, id, after, runs, go }) {
  const { t } = useTranslation();
  const toast = useToast();
  const [busy, setBusy] = useState('');
  if (!hist.length) return null;
  const latest = hist.find((c) => !c.undone && c.op !== 'undo' && !c.legacy);
  const act = (c, action) => {
    setBusy(c.id);
    api.post(`/assistant/memory/changes/${c.id}/${action}`).then(
      () => (setBusy(''), after()),
      (e) => (setBusy(''), toast(errorText(t, e), 'error')),
    );
  };
  return (
    <>
      <h4 className="mem-sec">{t('asst.mem.history')}</h4>
      <ol className="mem-hist">
        {hist.map((c, i) => {
          const diff = c.op === 'rename' ? [] : lineDiff(c.before, c.after);
          const fa = frontOf(c.before);
          const fb = frontOf(c.after);
          const front = c.before != null && c.after != null && (fa.description !== fb.description || fa.aliases !== fb.aliases);
          const run = c.run && runs?.find((r) => r.id === c.run);
          return (
            <li key={c.id} data-undone={c.undone ? '' : undefined}>
              <div className="mem-hist-top">
                <b>{t(`asst.mem.by.${c.by}`, { defaultValue: c.by })}</b>
                <span className="muted">{ago(t, c.at)}</span>
                <span>
                  {c.op === 'rename' ? t('asst.mem.op_rename', { from: stemOf(c.from || ''), to: stemOf(c.page) }) : t(`asst.mem.op.${c.op}`, { defaultValue: c.op })}
                  {front && ` · ${t('asst.mem.op_front')}`}
                </span>
                {c.undone && <span className="muted">{t('asst.mem.undone')}</span>}
              </div>
              {c.why && c.by !== 'user' && <span className="muted">{c.why}</span>}
              <Diff rows={diff} />
              <div className="mem-hist-top">
                <Basis list={c.basis} />
                {run && go && (
                  <button type="button" className="mem-lnk" onClick={() => go({ k: 'run', id: run.id })}>
                    {t('asst.mem.run_link')}
                  </button>
                )}
                {c === latest && c.op !== 'confirm' && (
                  <button type="button" className="mem-lnk" disabled={busy === c.id} onClick={() => act(c, 'undo')}>
                    {t('asst.mem.undo')}
                  </button>
                )}
                {i > 0 && c.after != null && !c.undone && c.page === id && (
                  <button type="button" className="mem-lnk" disabled={busy === c.id} onClick={() => act(c, 'revert')}>
                    {t('asst.mem.revert')}
                  </button>
                )}
              </div>
            </li>
          );
        })}
      </ol>
    </>
  );
}

/** 改一页：名字（= 改名）、说明、叫法（项目）、正文。改过的行算你写的 */
function EditView({ page, onCancel, onSaved }) {
  const { t } = useTranslation();
  const toast = useToast();
  const dir = dirOf(page.id);
  const [name, setName] = useState(page.name);
  const [desc, setDesc] = useState(page.description);
  const [aliases, setAliases] = useState((page.aliases || []).join('、'));
  const [body, setBody] = useState(bodyText(page.text));
  const [busy, setBusy] = useState(false);
  const submit = async (ev) => {
    ev.preventDefault();
    if (busy || !name.trim()) return;
    setBusy(true);
    try {
      let id = page.id;
      if (name.trim() !== page.name) {
        const r = await api.post('/assistant/memory/page/rename', { id, to: `${dir}/${name.trim()}` });
        id = r.page || id;
      }
      await api.put('/assistant/memory/page', { id, description: desc, aliases: splitAliases(aliases), body });
      onSaved(id);
    } catch (e) {
      setBusy(false);
      toast(errorText(t, e), 'error');
    }
  };
  return (
    <>
      <Head title={t('asst.mem.edit_title', { name: page.name })} onBack={onCancel} />
      <form className="mem-form" onSubmit={submit}>
        <label>
          {t('asst.mem.f_name')}
          <input value={name} maxLength={60} onChange={(e) => setName(e.target.value)} />
        </label>
        <label>
          {t('asst.mem.f_desc')}
          <input value={desc} maxLength={200} onChange={(e) => setDesc(e.target.value)} />
        </label>
        {dir === 'projects' && (
          <label>
            {t('asst.mem.f_aliases')}
            <input value={aliases} onChange={(e) => setAliases(e.target.value)} />
          </label>
        )}
        <label>
          {t('asst.mem.f_body')}
          <textarea className="mem-textarea" value={body} onChange={(e) => setBody(e.target.value)} rows={10} />
        </label>
        <div className="mem-form-foot">
          <span className="muted asst-small">{t(dir === 'you' ? 'asst.mem.edit_hint_you' : 'asst.mem.edit_hint')}</span>
          <Button variant="plain" className="btn-sm" onClick={onCancel}>
            {t('action.cancel')}
          </Button>
          <Button className="btn-sm" type="submit" disabled={!name.trim()} busy={busy}>
            {t('action.save')}
          </Button>
        </div>
      </form>
    </>
  );
}

function NewView({ go, onBack, reload }) {
  const { t } = useTranslation();
  const toast = useToast();
  const [dir, setDir] = useState('you');
  const [name, setName] = useState('');
  const [desc, setDesc] = useState('');
  const [aliases, setAliases] = useState('');
  const [body, setBody] = useState('');
  const [busy, setBusy] = useState(false);
  const ok = name.trim() && body.trim();
  const submit = (ev) => {
    ev.preventDefault();
    if (!ok || busy) return;
    setBusy(true);
    api.put('/assistant/memory/page', { id: `${dir}/${name.trim()}`, create: true, description: desc, aliases: dir === 'projects' ? splitAliases(aliases) : [], body }).then(
      (r) => (reload(), go({ k: 'page', id: r.page })),
      (e) => (setBusy(false), toast(errorText(t, e), 'error')),
    );
  };
  return (
    <>
      <Head title={t('asst.mem.new_page')} onBack={onBack} />
      <form className="mem-form" onSubmit={submit}>
        <div className="mem-chips" role="group" aria-label={t('asst.mem.f_dir')}>
          {DIRS.map((d) => (
            <button key={d} type="button" className="chip" aria-pressed={dir === d} onClick={() => setDir(d)}>
              {t(`asst.mem.dir_name.${d}`)}
            </button>
          ))}
        </div>
        <p className="muted asst-small">{t(`asst.mem.dir_hint.${dir}`)}</p>
        <label>
          {t('asst.mem.f_name')}
          <input value={name} maxLength={60} onChange={(e) => setName(e.target.value)} placeholder={t(dir === 'projects' ? 'asst.mem.f_name_ph_project' : 'asst.mem.f_name_ph')} autoFocus />
        </label>
        <label>
          {t('asst.mem.f_desc')}
          <input value={desc} maxLength={200} onChange={(e) => setDesc(e.target.value)} />
        </label>
        {dir === 'projects' && (
          <label>
            {t('asst.mem.f_aliases')}
            <input value={aliases} onChange={(e) => setAliases(e.target.value)} />
          </label>
        )}
        <label>
          {t('asst.mem.f_body')}
          <textarea className="mem-textarea" value={body} onChange={(e) => setBody(e.target.value)} rows={8} placeholder={t('asst.mem.f_body_ph')} />
        </label>
        <div className="mem-form-foot">
          <span />
          <Button variant="plain" className="btn-sm" onClick={onBack}>
            {t('action.cancel')}
          </Button>
          <Button className="btn-sm" type="submit" disabled={!ok} busy={busy}>
            {t('action.save')}
          </Button>
        </div>
      </form>
    </>
  );
}

// ---------------- 一次整理 ----------------

/** 读了什么：对话几句、会话几个、流水几行、你编程会话里你说的几句 */
function readText(t, read) {
  if (!read) return '';
  return ['said', 'events', 'journal', 'coding']
    .filter((k) => read[k] > 0)
    .map((k) => t(`asst.mem.read_${k}`, { count: read[k] }))
    .join(t('asst.mem.read_sep'));
}

// 没采用的理由是写给模型看的英文（assistant_memory.rs apply_run / assistant_jobs.rs 核对依据），常见的换成人话
const DROP_WHY = [
  [/^no basis|basis is not in/, 'asst.mem.drop.basis'],
  [/same page twice/, 'asst.mem.drop.twice'],
  [/changed while tidying/, 'asst.mem.drop.changed'],
  [/^no page|^no such/, 'asst.mem.drop.gone'],
  [/already exists/, 'error:MEMORY_EXISTS'],
  [/^too long/, 'error:MEMORY_TOO_LONG'],
  [/looks like a key/, 'error:MEMORY_SECRET'],
  [/^bad page path/, 'error:MEMORY_BAD_NAME'],
  [/^memory is full/, 'error:MEMORY_FULL'],
];
const DROP_OP = { write: 'edit', delete: 'delete', rename: 'rename' };
const dropWhy = (t, why) => {
  const hit = DROP_WHY.find(([re]) => re.test(why || ''));
  return hit ? t(hit[1]) : why;
};

function usageText(t, r) {
  const u = r.usage;
  const model = r.model || (r.engine === 'codex' ? 'Codex' : '');
  if (!u || u.input == null) return model;
  const k = (n) => (n >= 10_000 ? t('asst.mem.tok_w', { n: (n / 10_000).toFixed(1) }) : t('asst.mem.tok_k', { n: Math.max(1, Math.round(n / 1000)) }));
  const input = (u.input || 0) + (u.cache_read || 0) + (u.cache_write || 0);
  return [model, t('asst.mem.usage_io', { input: k(input), output: k(u.output || 0) })].filter(Boolean).join(' · ');
}

function RunView({ id, data, go, onBack, reload }) {
  const { t } = useTranslation();
  const toast = useToast();
  const [d, setD] = useState(null);
  const [busy, setBusy] = useState(false);
  const load = useCallback(() => api.get(`/assistant/memory/runs/${id}`).then(setD, (e) => toast(errorText(t, e), 'error')), [id, t, toast]);
  useEffect(() => {
    load();
  }, [load]);
  const after = () => (load(), reload());
  const undo = useRunUndo(d?.run || {}, after);
  if (!d) return <Head title={t('asst.mem.run_title')} onBack={onBack} />;
  const r = d.run;
  const apply = () => {
    setBusy(true);
    api.post(`/assistant/memory/runs/${r.id}/apply`).then(
      () => (setBusy(false), after()),
      (e) => (setBusy(false), toast(errorText(t, e), 'error')),
    );
  };
  const read = readText(t, r.read);
  return (
    <>
      <Head title={t('asst.mem.run_title_at', { when: ago(t, r.at) })} onBack={onBack} />
      <div className="mem-meta">
        <span>
          {runTitle(t, r)}
          {r.manual && ` · ${t('asst.mem.run_manual')}`}
        </span>
        {usageText(t, r) && <span>{usageText(t, r)}</span>}
        {read && <span>{t('asst.mem.read', { what: read })}</span>}
      </div>
      {r.status === 'held' && (
        <div className="mem-pend">
          <div className="mem-pend-h">
            <AlertTriangle size={14} aria-hidden="true" />
            {t(r.applied_at ? 'asst.mem.run_applied' : 'asst.mem.run_held')}
          </div>
          {!r.applied_at && <span>{t('asst.mem.run_held_body', { pct: Math.round((r.loss || 0) * 100) })}</span>}
          {!r.applied_at && (
            <div className="mem-acts-row">
              <Button className="btn-sm" busy={busy} onClick={apply}>
                {t('asst.mem.run_apply')}
              </Button>
            </div>
          )}
        </div>
      )}
      {r.error && <p className="mem-err asst-small">{r.error}</p>}
      {r.summary && <p className="mem-desc">{r.summary}</p>}
      {d.changes.length > 0 && (
        <>
          <h4 className="mem-sec">{t('asst.mem.run_changes')}</h4>
          {d.changes.map((c) => (
            <div key={c.id} className="mem-card" data-undone={c.undone ? '' : undefined}>
              <span>
                <button type="button" className="mem-lnk mem-lnk-strong" onClick={() => go({ k: 'page', id: c.page })}>
                  {stemOf(c.page)}
                </button>
                {' · '}
                {c.op === 'rename' ? t('asst.mem.op_rename', { from: stemOf(c.from || ''), to: stemOf(c.page) }) : t(`asst.mem.op.${c.op}`, { defaultValue: c.op })}
                {c.undone && ` · ${t('asst.mem.undone')}`}
              </span>
              {c.why && <span className="muted asst-small">{c.why}</span>}
              <Diff rows={c.op === 'rename' ? [] : lineDiff(c.before, c.after)} />
              <div className="mem-acts-row">
                <Basis list={c.basis} />
              </div>
            </div>
          ))}
        </>
      )}
      {r.status === 'held' && !r.applied_at && r.held?.length > 0 && (
        <>
          <h4 className="mem-sec">{t('asst.mem.run_would')}</h4>
          {r.held.map((o, i) => {
            const cur = data.pages.find((p) => p.id === o.page);
            return (
              <div key={i} className="mem-card">
                <span>
                  <b>{stemOf(o.page)}</b> · {t(`asst.mem.held_op.${o.op}`, { defaultValue: o.op, to: stemOf(o.to || '') })}
                  {!cur && o.op === 'write' && ` · ${t('asst.mem.held_new')}`}
                </span>
                {o.why && <span className="muted asst-small">{o.why}</span>}
                <div className="mem-acts-row">
                  <Basis list={o.basis} />
                </div>
              </div>
            );
          })}
        </>
      )}
      {d.pending.length > 0 && (
        <>
          <h4 className="mem-sec">{t('asst.mem.run_pending_h')}</h4>
          {d.pending.map((p) => (
            <button key={p.id} type="button" className="mem-flag" onClick={() => go({ k: 'page', id: p.page, pend: true })}>
              <AlertTriangle size={14} aria-hidden="true" />
              <span className="ellipsis">{t('asst.mem.run_pending_line', { name: stemOf(p.page) })}</span>
              <ChevronRight size={14} aria-hidden="true" />
            </button>
          ))}
        </>
      )}
      {r.dropped?.length > 0 && (
        <>
          <h4 className="mem-sec">{t('asst.mem.run_dropped_h')}</h4>
          {r.dropped.map((x, i) => (
            <div key={i} className="mem-card">
              <span>
                <b>{stemOf(x.page) || x.page}</b> · {t(`asst.mem.op.${DROP_OP[x.what.split(' ')[0]] || 'edit'}`)}
              </span>
              <span className="muted asst-small">{dropWhy(t, x.why)}</span>
            </div>
          ))}
        </>
      )}
      {undo.can && (
        <div className="mem-acts-row">
          <Button variant="secondary" className="btn-sm" busy={undo.busy} onClick={undo.run}>
            <Undo2 size={14} aria-hidden="true" />
            {t('asst.mem.run_undo')}
          </Button>
        </div>
      )}
      <p className="muted asst-small">{t('asst.mem.run_note')}</p>
    </>
  );
}

// ---------------- 设置 ----------------

/** 每晚整理（开关、模型、读不读编程会话、现在整理）、“关于你”同步、搜索索引、导出 */
function MemorySettings({ data, reload, go }) {
  const { t } = useTranslation();
  const toast = useToast();
  const [busy, setBusy] = useState('');
  const j = data.jobs;
  const set = (b) => api.post('/assistant/memory-settings', b).then(reload, (e) => toast(errorText(t, e), 'error'));
  const dream = () => {
    setBusy('dream');
    api.post('/assistant/memory-jobs/dream').then(
      (r) => {
        setBusy('');
        reload();
        const run = r.run;
        if (run?.changes?.length || run?.held?.length || run?.pending?.length) go({ k: 'run', id: run.id });
        else toast(run?.summary || t('asst.mem.run_none'));
      },
      (e) => (toast(errorText(t, e), 'error'), setBusy('')),
    );
  };
  const rebuild = () => {
    setBusy('index');
    api.post('/assistant/memory-jobs/index').then(
      () => (toast(t('asst.mem.rebuilt')), setBusy(''), reload()),
      (e) => (toast(errorText(t, e), 'error'), setBusy('')),
    );
  };
  const exportZip = async () => {
    setBusy('export');
    try {
      const res = await fetchRaw('/assistant/memory-export');
      const name = /filename="([^"]+)"/.exec(res.headers.get('Content-Disposition') || '')?.[1] || 'looklook-assistant.zip';
      const url = URL.createObjectURL(await res.blob());
      Object.assign(document.createElement('a'), { href: url, download: name }).click();
      setTimeout(() => URL.revokeObjectURL(url), 60_000);
    } catch (e) {
      toast(errorText(t, e), 'error');
    }
    setBusy('');
  };
  return (
    <details className="mem-more">
      <summary>{t('asst.mem.settings')}</summary>
      <div className="mem-settings">
        <div className="mem-setting">
          <div>
            <div className="mem-setting-name">{t('asst.mem.jobs')}</div>
            <div className="muted asst-small">{t('asst.mem.jobs_hint')}</div>
            <div className="muted asst-small">{j.dreamed ? t('asst.mem.dreamed', { when: ago(t, j.dreamed) }) : t('asst.mem.dream_never')}</div>
          </div>
          <Switch checked={j.enabled} onChange={(v) => set({ jobs: v })} label={t('asst.mem.jobs')} />
        </div>
        <div className="mem-setting mem-setting-col">
          <div>
            <div className="mem-setting-name">{t('asst.mem.model')}</div>
            <div className="muted asst-small">{t('asst.mem.model_hint')}</div>
          </div>
          <Select value={j.model || 'auto'} onChange={(v) => set({ model: v })} options={['auto', 'strong', 'brain'].map((m) => [m, t(`asst.mem.model_${m}`)])} aria-label={t('asst.mem.model')} />
        </div>
        <div className="mem-setting">
          <div>
            <div className="mem-setting-name">{t('asst.mem.own')}</div>
            <div className="muted asst-small">{t('asst.mem.own_hint')}</div>
          </div>
          <Switch checked={j.own_sessions} onChange={(v) => set({ own_sessions: v })} label={t('asst.mem.own')} />
        </div>
        <div className="mem-setting">
          <div>
            <div className="mem-setting-name">{t('asst.mem.sync')}</div>
            <div className="muted asst-small">{t('asst.mem.sync_hint')}</div>
          </div>
          <Switch checked={data.sync} onChange={(v) => set({ sync: v })} label={t('asst.mem.sync')} />
        </div>
        <div className="mem-setting-btns">
          <Button variant="secondary" className="btn-sm" busy={busy === 'dream'} disabled={Boolean(busy) || j.running} onClick={dream}>
            <Moon size={14} aria-hidden="true" />
            {busy === 'dream' ? t('asst.mem.dreaming') : t('asst.mem.dream_now')}
          </Button>
          <Button variant="plain" className="btn-sm" busy={busy === 'index'} disabled={Boolean(busy)} onClick={rebuild} title={t('asst.mem.index_hint', { docs: data.index?.docs ?? 0, files: data.index?.files ?? 0, imported: data.imported })}>
            <RefreshCw size={14} aria-hidden="true" />
            {t('asst.mem.rebuild')}
          </Button>
          <Button variant="plain" className="btn-sm" busy={busy === 'export'} disabled={Boolean(busy)} onClick={exportZip} title={t('asst.mem.export_hint')}>
            <Download size={14} aria-hidden="true" />
            {t('asst.mem.export_btn')}
          </Button>
        </div>
        <p className="muted asst-small">{t('asst.mem.index_hint', { docs: data.index?.docs ?? 0, files: data.index?.files ?? 0, imported: data.imported })}</p>
        <p className="muted asst-small mem-dir">{t('asst.mem.dir', { dir: data.dir })}</p>
      </div>
    </details>
  );
}

