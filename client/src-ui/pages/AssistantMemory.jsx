// 助理的记忆页（docs/ASSISTANT.md「记忆」，设计稿 docs/SINGLE_ENTRY_AGENT_DESIGN_2026-11-11.md §5）：
// 等你点头的提议、四类条目、项目名册、全文搜索（记忆 + 流水 + 过去的对话 + 导进来的 Claude Code / Codex 记忆）、
// 每条的历史版本、彻底忘掉、同步/后台整理开关、导出。
import { ArchiveRestore, Check, Download, FolderGit2, History, Pencil, Pin, Plus, RefreshCw, Search, Trash2, Undo2, X } from 'lucide-react';
import { useCallback, useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';

import { api, fetchRaw } from '../shared/api.js';
import { errorText } from '../shared/i18n.js';
import Select from '../shared/Select.jsx';
import { baseName } from '../shared/sessions.jsx';
import { relativeTime } from '../shared/time.js';
import { Button, Switch, useConfirm, useToast } from '../shared/ui.jsx';

// 记忆的四类（assistant_memory.rs 的 Kind），页面上按这个顺序分组
export const MEM_KINDS = ['me', 'lesson', 'project', 'note'];
const MEM_TEXT_MAX = 600;
const MEM_TITLE_MAX = 80;
// 搜到的哪一类（assistant_index.rs 的 src）
// 提议多的时候（第一次启用找到十几个项目）先列几条
const PROPOSALS_SHOWN = 3;
const SRC_ORDER = ['memory', 'archive', 'journal', 'assistant', 'claude', 'codex', 'imported'];

/** 搜到的一段是谁的：记忆 = 哪一类；对话 = 你说 / 回答 */
function hitRole(t, h) {
  if (h.src === 'memory' || h.src === 'archive') return MEM_KINDS.includes(h.role) ? t(`asst.mem.kind.${h.role}`) : '';
  return h.role === 'user' || h.role === 'assistant' ? t(`asst.mem.role.${h.role}`) : '';
}

/** 后台复盘、整理的结果 → 一句话 */
function jobText(t, r) {
  if (!r) return '';
  if (r.error) return t('asst.mem.job_failed', { error: r.error });
  if (r.seeded !== undefined) return t('asst.mem.job_seeded', { count: r.seeded });
  if (r.rebuilt) return t('asst.mem.job_rebuilt');
  const parts = ['saved', 'archived', 'proposed'].filter((k) => r[k] > 0).map((k) => t(`asst.mem.job_${k}`, { count: r[k] }));
  return parts.length ? parts.join(t('asst.mem.job_sep')) : t('asst.mem.job_none');
}

/** 变化是怎么来的：你 / 助理 / 后台复盘 / 每日整理 / 别的电脑 … */
export function changeBy(t, c) {
  if (c.via) return t(`asst.mem.via.${c.via}`, { defaultValue: t('asst.mem.by_assistant') });
  return t(c.by === 'user' ? 'asst.mem.by_you' : 'asst.mem.by_assistant');
}

/**
 * 记忆：助理记着的关于你、你的项目的事。每次起大脑时带上（钉住的、你写的优先），你在这里改的下一轮就告诉它。
 * 能加、改、钉住、忘掉（进归档能拿回来）、看每一版并回到某一版、彻底忘掉。
 */
export default function MemoryTab() {
  const { t } = useTranslation();
  const toast = useToast();
  const confirm = useConfirm();
  const [data, setData] = useState(null);
  const [adding, setAdding] = useState(null);
  const [editing, setEditing] = useState(null);
  const load = useCallback(() => api.get('/assistant/memory').then(setData, (e) => toast(errorText(t, e), 'error')), [t, toast]);
  useEffect(() => {
    load();
  }, [load]);
  const act = (p) => p.then(load, (e) => toast(errorText(t, e), 'error'));
  const purge = async (e) => {
    const ok = await confirm({ title: t('asst.mem.purge_q', { title: e.title }), body: t(e.kind === 'me' ? 'asst.mem.purge_body_me' : 'asst.mem.purge_body'), confirm: t('asst.mem.purge'), danger: true });
    if (ok) act(api.post(`/assistant/memory/${e.id}/purge`));
  };
  if (!data) return <div className="mem" aria-busy="true" />;
  const r = data.resident;
  return (
    <section className="mem">
      <p className="muted asst-small">{t('asst.mem.hint')}</p>
      <div className="mem-bar">
        <span className="muted asst-small mem-usage" title={t('asst.mem.usage_tip')}>
          {t('asst.mem.usage', { used: r.me_used, budget: r.me_budget })}
          {r.left_out > 0 && ` · ${t('asst.mem.left_out', { count: r.left_out })}`}
        </span>
        {!adding && (
          <Button variant="secondary" className="btn-sm" onClick={() => setAdding({})}>
            <Plus size={14} aria-hidden="true" />
            {t('asst.mem.add')}
          </Button>
        )}
      </div>
      {adding && <MemoryForm entry={adding.kind ? adding : null} fresh projects={data.projects} onCancel={() => setAdding(null)} onSave={(b) => api.post('/assistant/memory', b).then(() => (setAdding(null), load()))} />}
      <Proposals list={data.proposals} onDone={load} />
      <MemorySearch entries={data.entries} onAdd={(x) => setAdding(x)} onChanged={load} />
      {data.entries.length === 0 && !adding && <p className="muted asst-small asst-empty">{t('asst.mem.empty')}</p>}
      {MEM_KINDS.map((k) => {
        const list = data.entries.filter((e) => e.kind === k).sort((a, b) => b.pinned - a.pinned || b.updated - a.updated);
        if (!list.length) return null;
        return (
          <div key={k} className="mem-group">
            <h3 className="asst-h asst-h-sm">
              {t(`asst.mem.kind.${k}`)}
              <span className="asst-count-soft">{list.length}</span>
            </h3>
            <ul className="mem-list">
              {list.map((e) =>
                editing === e.id ? (
                  <li key={e.id} className="mem-item mem-item-edit">
                    <MemoryForm entry={e} projects={data.projects} onCancel={() => setEditing(null)} onSave={(b) => api.put(`/assistant/memory/${e.id}`, b).then(() => (setEditing(null), load()))} />
                  </li>
                ) : (
                  <MemoryItem
                    key={e.id}
                    e={e}
                    onEdit={() => setEditing(e.id)}
                    onPin={() => act(api.put(`/assistant/memory/${e.id}`, { pinned: !e.pinned }))}
                    onDrop={() => act(api.del(`/assistant/memory/${e.id}`))}
                    onPurge={() => purge(e)}
                    onChanged={load}
                  />
                ),
              )}
            </ul>
          </div>
        );
      })}
      <Roster list={data.roster} projects={data.projects} leftOut={r.roster_left_out} onChanged={load} />
      {data.changes.length > 0 && (
        <details className="mem-more">
          <summary>{t('asst.mem.changes')}</summary>
          <ul className="mem-changes">
            {data.changes.map((c, i) => {
              const e = c.after || c.before;
              // 新的在前：这条之后同一条记忆又变过的，要先撤后面那次
              const canUndo = !c.undone && c.op !== 'undo' && !data.changes.slice(0, i).some((x) => x.entry === c.entry && !x.undone && x.op !== 'undo');
              return (
                <li key={c.id} data-undone={c.undone ? '' : undefined}>
                  <span className="mem-change-what">
                    <span className="mem-change-op">{t(`asst.mem.op.${c.op}`, { defaultValue: c.op })}</span>
                    {e?.title}
                    <span className="muted">
                      {' · '}
                      {changeBy(t, c)} · {relativeTime(t, c.at)}
                      {c.undone && ` · ${t('asst.mem.undone')}`}
                    </span>
                    {c.why && <span className="muted mem-change-why">{t('asst.mem.because', { why: c.why })}</span>}
                  </span>
                  {canUndo && (
                    <button type="button" className="btn btn-plain btn-icon btn-sm" onClick={() => act(api.post(`/assistant/memory/changes/${c.id}/undo`))} aria-label={t('asst.mem.undo')} title={t('asst.mem.undo')}>
                      <Undo2 size={14} aria-hidden="true" />
                    </button>
                  )}
                </li>
              );
            })}
          </ul>
        </details>
      )}
      {data.archived.length > 0 && (
        <details className="mem-more">
          <summary>{t('asst.mem.archived', { count: data.archived.length })}</summary>
          <ul className="mem-list">
            {data.archived.map((e) => (
              <li key={e.id} className="mem-item mem-item-archived">
                <div className="mem-body">
                  <div className="mem-title">{e.title}</div>
                  <div className="mem-text muted">{e.text}</div>
                </div>
                <span className="mem-acts">
                  <button type="button" className="btn btn-plain btn-icon btn-sm" onClick={() => act(api.post(`/assistant/memory/${e.id}/restore`))} aria-label={t('asst.mem.restore')} title={t('asst.mem.restore')}>
                    <ArchiveRestore size={14} aria-hidden="true" />
                  </button>
                  <button type="button" className="btn btn-plain btn-icon btn-sm" onClick={() => purge(e)} aria-label={t('asst.mem.purge')} title={t('asst.mem.purge')}>
                    <Trash2 size={14} aria-hidden="true" />
                  </button>
                </span>
              </li>
            ))}
          </ul>
        </details>
      )}
      <MemorySettings data={data} onChanged={load} />
      <p className="muted asst-small asst-foot mem-dir">{t('asst.mem.dir', { dir: data.dir })}</p>
    </section>
  );
}

/** 等你点头的：后台整理想合并、归档的，助理想改你写的，第一次启用找到的项目 */
function Proposals({ list, onDone }) {
  const { t } = useTranslation();
  const toast = useToast();
  const [busy, setBusy] = useState('');
  const [all, setAll] = useState(false);
  if (!list?.length) return null;
  const run = async (ids, action) => {
    setBusy(ids.length > 1 ? `all:${action}` : `${ids[0]}:${action}`);
    try {
      for (const id of ids) await api.post(`/assistant/memory/proposals/${id}/${action}`);
    } catch (e) {
      toast(errorText(t, e), 'error');
    }
    setBusy('');
    onDone();
  };
  const seeds = list.filter((p) => p.from === 'seed');
  return (
    <div className="mem-group mem-proposals">
      <h3 className="asst-h asst-h-sm">
        {t('asst.mem.proposals')}
        <span className="asst-count-soft">{list.length}</span>
        {seeds.length > 1 && (
          <Button variant="plain" className="btn-sm mem-h-act" busy={busy === 'all:accept'} onClick={() => run(seeds.map((p) => p.id), 'accept')}>
            {t('asst.mem.seed_all', { count: seeds.length })}
          </Button>
        )}
      </h3>
      <ul className="mem-list">
        {(all ? list : list.slice(0, PROPOSALS_SHOWN)).map((p) => (
          <li key={p.id} className="mem-item mem-proposal">
            <div className="mem-body">
              <span className="mem-title">
                {p.summary}
                <span className="mem-tag">{t(`asst.mem.from.${p.from}`, { defaultValue: p.from })}</span>
              </span>
              {p.why && <span className="mem-text muted">{p.why}</span>}
            </div>
            <span className="mem-acts mem-acts-on">
              <button type="button" className="btn btn-plain btn-icon btn-sm" disabled={Boolean(busy)} onClick={() => run([p.id], 'accept')} aria-label={t('asst.mem.accept')} title={t('asst.mem.accept')}>
                <Check size={15} aria-hidden="true" />
              </button>
              <button type="button" className="btn btn-plain btn-icon btn-sm" disabled={Boolean(busy)} onClick={() => run([p.id], 'dismiss')} aria-label={t('asst.mem.dismiss')} title={t('asst.mem.dismiss')}>
                <X size={15} aria-hidden="true" />
              </button>
            </span>
          </li>
        ))}
      </ul>
      {!all && list.length > PROPOSALS_SHOWN && (
        <Button variant="plain" className="btn-sm mem-show-more" onClick={() => setAll(true)}>
          {t('asst.mem.proposals_more', { count: list.length - PROPOSALS_SHOWN })}
        </Button>
      )}
    </div>
  );
}

/** 在记忆里找：打字先筛当前条目；回车再搜过去的一切（流水、对话、导进来的记忆） */
function MemorySearch({ entries, onAdd, onChanged }) {
  const { t } = useTranslation();
  const toast = useToast();
  const confirm = useConfirm();
  const [q, setQ] = useState('');
  const [hits, setHits] = useState(null);
  const [busy, setBusy] = useState(false);
  const needle = q.trim().toLowerCase();
  const local = needle ? entries.filter((e) => `${e.title}\n${e.text}\n${e.project || ''}`.toLowerCase().includes(needle)) : [];
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
    const inPlace = ['memory', 'archive', 'journal', 'assistant'].includes(h.src);
    const ok = await confirm({ title: t('asst.mem.forget_hit_q'), body: t(inPlace ? 'asst.mem.forget_hit_body' : 'asst.mem.forget_hit_body_index'), confirm: t('asst.mem.purge'), danger: true });
    if (!ok) return;
    try {
      const r = await api.post(`/assistant/memory-search/${h.h}/forget`);
      setHits((l) => l.filter((x) => x.h !== h.h));
      toast(t(r.source === 'removed' ? 'asst.mem.forgot_hit' : 'asst.mem.forgot_hit_index'));
      onChanged();
    } catch (e) {
      toast(errorText(t, e), 'error');
    }
  };
  const groups = hits ? SRC_ORDER.map((s) => [s, hits.filter((h) => h.src === s)]).filter(([, l]) => l.length) : [];
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
      {needle && !hits && local.length > 0 && <p className="muted asst-small">{t('asst.mem.filter_n', { count: local.length })}</p>}
      {hits && hits.length === 0 && <p className="muted asst-small">{t('asst.mem.search_none')}</p>}
      {groups.map(([src, list]) => (
        <div key={src} className="mem-hits">
          <h4 className="mem-hits-h">{t(`asst.mem.src.${src}`)}</h4>
          <ul className="mem-list">
            {list.map((h) => (
              <li key={h.h} className="mem-item">
                <div className="mem-body">
                  <span className="mem-text muted mem-hit-meta">
                    {[h.title, h.workdir && baseName(h.workdir), hitRole(t, h), h.at ? relativeTime(t, h.at) : src === 'journal' ? h.ref : ''].filter(Boolean).join(' · ')}
                  </span>
                  <span className="mem-text" data-open="">
                    {h.snippet}
                  </span>
                </div>
                <span className="mem-acts mem-acts-on">
                  {src === 'imported' && (
                    <button type="button" className="btn btn-plain btn-icon btn-sm" onClick={() => onAdd({ kind: 'note', title: h.title.split(' · ').pop().slice(0, MEM_TITLE_MAX), text: h.snippet.slice(0, MEM_TEXT_MAX), source: h.title })} aria-label={t('asst.mem.promote')} title={t('asst.mem.promote')}>
                      <Plus size={14} aria-hidden="true" />
                    </button>
                  )}
                  <button type="button" className="btn btn-plain btn-icon btn-sm" onClick={() => forget(h)} aria-label={t('asst.mem.purge')} title={t('asst.mem.purge')}>
                    <Trash2 size={14} aria-hidden="true" />
                  </button>
                </span>
              </li>
            ))}
          </ul>
        </div>
      ))}
    </div>
  );
}

function MemoryItem({ e, onEdit, onPin, onDrop, onPurge, onChanged }) {
  const { t } = useTranslation();
  const toast = useToast();
  const [open, setOpen] = useState(false);
  const [versions, setVersions] = useState(null);
  const loadVersions = () => api.get(`/assistant/memory/${e.id}/versions`).then((r) => setVersions(r.versions), (err) => toast(errorText(t, err), 'error'));
  const revert = (c) =>
    api.post(`/assistant/memory/changes/${c.id}/revert`).then(
      () => (setVersions(null), onChanged()),
      (err) => toast(errorText(t, err), 'error'),
    );
  return (
    <li className="mem-item mem-item-col" data-pinned={e.pinned ? '' : undefined} data-open={open ? '' : undefined}>
      <div className="mem-main">
        <button type="button" className="mem-body" onClick={() => setOpen((v) => !v)} aria-expanded={open}>
          <span className="mem-title">
            {e.pinned && <Pin size={12} aria-hidden="true" className="mem-pin-ico" />}
            {e.title}
            {e.project && <span className="mem-tag">{baseName(e.project)}</span>}
            {e.by === 'user' && <span className="mem-tag">{t('asst.mem.yours')}</span>}
          </span>
          <span className="mem-text muted" data-open={open ? '' : undefined}>
            {e.text}
          </span>
          {open && e.source && <span className="mem-text muted">{t('asst.mem.source', { source: e.source })}</span>}
        </button>
        <span className="mem-acts">
          <button type="button" className="btn btn-plain btn-icon btn-sm" onClick={onPin} aria-pressed={e.pinned} aria-label={t(e.pinned ? 'asst.mem.unpin' : 'asst.mem.pin')} title={t(e.pinned ? 'asst.mem.unpin' : 'asst.mem.pin')}>
            <Pin size={14} aria-hidden="true" className={e.pinned ? 'mem-pin-on' : undefined} />
          </button>
          <button type="button" className="btn btn-plain btn-icon btn-sm" onClick={onEdit} aria-label={t('asst.mem.edit')} title={t('asst.mem.edit')}>
            <Pencil size={14} aria-hidden="true" />
          </button>
          <button type="button" className="btn btn-plain btn-icon btn-sm" onClick={onDrop} aria-label={t('asst.mem.forget')} title={t('asst.mem.forget')}>
            <ArchiveRestore size={14} aria-hidden="true" className="mem-ico-archive" />
          </button>
        </span>
      </div>
      {open && (
        <div className="mem-more-acts">
          <span className="muted asst-small">
            {t(e.by === 'user' ? 'asst.mem.by_you' : 'asst.mem.by_assistant')} · {relativeTime(t, e.updated)}
            {e.used_at > 0 && ` · ${t('asst.mem.used', { when: relativeTime(t, e.used_at) })}`}
          </span>
          <Button variant="plain" className="btn-sm" onClick={() => (versions ? setVersions(null) : loadVersions())} aria-expanded={Boolean(versions)}>
            <History size={14} aria-hidden="true" />
            {t('asst.mem.versions')}
          </Button>
          <Button variant="plain" className="btn-sm mem-danger" onClick={onPurge}>
            <Trash2 size={14} aria-hidden="true" />
            {t('asst.mem.purge')}
          </Button>
        </div>
      )}
      {open && versions && (
        <ol className="mem-versions">
          {versions.map((c, i) => (
            <li key={c.id}>
              <span className="mem-change-what">
                <span className="mem-change-op">{t(`asst.mem.op.${c.op}`, { defaultValue: c.op })}</span>
                <span className="muted">
                  {changeBy(t, c)} · {relativeTime(t, c.at)}
                </span>
                {c.after && <span className="mem-text muted mem-version-text">{c.after.title !== e.title ? `${c.after.title}：${c.after.text}` : c.after.text}</span>}
              </span>
              {i > 0 && c.after && !c.after.archived && (
                <Button variant="plain" className="btn-sm" onClick={() => revert(c)}>
                  {t('asst.mem.revert')}
                </Button>
              )}
            </li>
          ))}
        </ol>
      )}
    </li>
  );
}

/** 加一条、改一条（`fresh` + entry = 从搜到的导入记忆预填的新条目） */
function MemoryForm({ entry, fresh, projects, onSave, onCancel }) {
  const { t } = useTranslation();
  const toast = useToast();
  const [kind, setKind] = useState(entry?.kind || 'me');
  const [title, setTitle] = useState(entry?.title || '');
  const [text, setText] = useState(entry?.text || '');
  const [project, setProject] = useState(entry?.project || projects[0]?.path || '');
  const [busy, setBusy] = useState(false);
  const needsProject = kind === 'project';
  const ok = title.trim() && text.trim() && text.length <= MEM_TEXT_MAX && (!needsProject || project);
  const submit = (ev) => {
    ev.preventDefault();
    if (!ok || busy) return;
    setBusy(true);
    const body = { kind, title, text };
    if (needsProject || (entry?.project && !fresh)) body.project = needsProject ? project : '';
    onSave(body).catch((e) => {
      setBusy(false);
      toast(errorText(t, e), 'error');
    });
  };
  const projectOptions = useMemo(() => {
    const list = projects.map((p) => [p.path, p.name]);
    if (project && !list.some(([v]) => v === project)) list.unshift([project, baseName(project)]);
    return list;
  }, [projects, project]);
  return (
    <form className="mem-form" onSubmit={submit}>
      <div className="mem-kinds" role="group" aria-label={t('asst.mem.kind_label')}>
        {MEM_KINDS.map((k) => (
          <button key={k} type="button" aria-pressed={kind === k} className="chip" onClick={() => setKind(k)}>
            {t(`asst.mem.kind.${k}`)}
          </button>
        ))}
      </div>
      <p className="muted asst-small">{t(`asst.mem.kind_hint.${kind}`)}</p>
      {fresh && entry?.source && <p className="muted asst-small">{t('asst.mem.source', { source: entry.source })}</p>}
      {needsProject && <Select value={project} onChange={setProject} options={projectOptions} aria-label={t('asst.mem.project')} />}
      <input value={title} maxLength={MEM_TITLE_MAX} onChange={(e) => setTitle(e.target.value)} placeholder={t('asst.mem.title_ph')} aria-label={t('asst.mem.title')} autoFocus />
      <textarea className="mem-textarea" value={text} onChange={(e) => setText(e.target.value)} placeholder={t(`asst.mem.text_ph.${kind}`)} aria-label={t('asst.mem.text')} rows={3} />
      <div className="mem-form-foot">
        <span className="muted asst-small" data-over={text.length > MEM_TEXT_MAX ? '' : undefined}>
          {text.length}/{MEM_TEXT_MAX}
        </span>
        <Button variant="plain" className="btn-sm" onClick={onCancel}>
          {t('action.cancel')}
        </Button>
        <Button className="btn-sm" type="submit" disabled={!ok} busy={busy}>
          {t('action.save')}
        </Button>
      </div>
    </form>
  );
}

/** 项目名册：你怎么叫各个项目、一句话是干什么的（常驻，助理靠它认出你说的是哪个项目） */
function Roster({ list, projects, leftOut, onChanged }) {
  const { t } = useTranslation();
  const toast = useToast();
  const [editing, setEditing] = useState(null);
  const save = (b) => api.post('/assistant/memory-roster', b).then(() => (setEditing(null), onChanged()));
  const remove = (path) => api.post('/assistant/memory-roster', { path, remove: true }).then(onChanged, (e) => toast(errorText(t, e), 'error'));
  const free = projects.filter((p) => !list.some((r) => r.path === p.path));
  return (
    <div className="mem-group">
      <h3 className="asst-h asst-h-sm">
        {t('asst.mem.roster')}
        <span className="asst-count-soft">{list.length}</span>
        {editing !== 'new' && free.length > 0 && (
          <Button variant="plain" className="btn-sm mem-h-act" onClick={() => setEditing('new')}>
            <Plus size={14} aria-hidden="true" />
            {t('asst.mem.roster_add')}
          </Button>
        )}
      </h3>
      <p className="muted asst-small">
        {t('asst.mem.roster_hint')}
        {leftOut > 0 && ` ${t('asst.mem.roster_left_out', { count: leftOut })}`}
      </p>
      {editing === 'new' && (
        <div className="mem-item mem-item-edit">
          <RosterForm projects={free} onSave={save} onCancel={() => setEditing(null)} />
        </div>
      )}
      {list.length > 0 && (
        <ul className="mem-list">
          {list.map((r) =>
            editing === r.path ? (
              <li key={r.path} className="mem-item mem-item-edit">
                <RosterForm item={r} projects={projects} onSave={save} onCancel={() => setEditing(null)} />
              </li>
            ) : (
              <li key={r.path} className="mem-item">
                <div className="mem-body mem-body-static">
                  <span className="mem-title">
                    <FolderGit2 size={13} aria-hidden="true" className="mem-roster-ico" />
                    {r.name}
                    {(r.aliases || []).filter((a) => a !== r.name).map((a) => (
                      <span key={a} className="mem-tag">
                        {a}
                      </span>
                    ))}
                  </span>
                  {r.about && <span className="mem-text muted">{r.about}</span>}
                  <span className="mem-text muted mem-path">{r.path}</span>
                </div>
                <span className="mem-acts">
                  <button type="button" className="btn btn-plain btn-icon btn-sm" onClick={() => setEditing(r.path)} aria-label={t('asst.mem.edit')} title={t('asst.mem.edit')}>
                    <Pencil size={14} aria-hidden="true" />
                  </button>
                  <button type="button" className="btn btn-plain btn-icon btn-sm" onClick={() => remove(r.path)} aria-label={t('asst.mem.roster_remove')} title={t('asst.mem.roster_remove')}>
                    <X size={14} aria-hidden="true" />
                  </button>
                </span>
              </li>
            ),
          )}
        </ul>
      )}
    </div>
  );
}

function RosterForm({ item, projects, onSave, onCancel }) {
  const { t } = useTranslation();
  const toast = useToast();
  const [path, setPath] = useState(item?.path || projects[0]?.path || '');
  const [name, setName] = useState(item?.name || '');
  const [aliases, setAliases] = useState((item?.aliases || []).join('、'));
  const [about, setAbout] = useState(item?.about || '');
  const [busy, setBusy] = useState(false);
  const submit = (ev) => {
    ev.preventDefault();
    if (!path || busy) return;
    setBusy(true);
    const list = aliases
      .split(/[,，、;；]/)
      .map((s) => s.trim())
      .filter(Boolean);
    onSave({ path, name: name.trim() || baseName(path), aliases: list, about: about.trim() }).catch((e) => {
      setBusy(false);
      toast(errorText(t, e), 'error');
    });
  };
  return (
    <form className="mem-form" onSubmit={submit}>
      {item ? <span className="muted asst-small mem-path">{item.path}</span> : <Select value={path} onChange={setPath} options={projects.map((p) => [p.path, p.name])} aria-label={t('asst.mem.project')} />}
      <input value={name} maxLength={40} onChange={(e) => setName(e.target.value)} placeholder={item ? item.name : baseName(path)} aria-label={t('asst.mem.roster_name')} />
      <input value={aliases} onChange={(e) => setAliases(e.target.value)} placeholder={t('asst.mem.roster_aliases_ph')} aria-label={t('asst.mem.roster_aliases')} />
      <input value={about} maxLength={200} onChange={(e) => setAbout(e.target.value)} placeholder={t('asst.mem.roster_about_ph')} aria-label={t('asst.mem.roster_about')} />
      <div className="mem-form-foot">
        <span />
        <Button variant="plain" className="btn-sm" onClick={onCancel}>
          {t('action.cancel')}
        </Button>
        <Button className="btn-sm" type="submit" disabled={!path} busy={busy}>
          {t('action.save')}
        </Button>
      </div>
    </form>
  );
}

/** 同步“关于你”、后台复盘/整理、搜索索引、导出 */
function MemorySettings({ data, onChanged }) {
  const { t } = useTranslation();
  const toast = useToast();
  const [busy, setBusy] = useState('');
  const j = data.jobs;
  const set = (b) => api.post('/assistant/memory-settings', b).then(onChanged, (e) => toast(errorText(t, e), 'error'));
  const run = (what) => {
    setBusy(what);
    api.post(`/assistant/memory-jobs/${what}`).then(
      (r) => (toast(jobText(t, r)), setBusy(''), onChanged()),
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
            <div className="mem-setting-name">{t('asst.mem.sync')}</div>
            <div className="muted asst-small">{t('asst.mem.sync_hint')}</div>
          </div>
          <Switch checked={data.sync} onChange={(v) => set({ sync: v })} label={t('asst.mem.sync')} />
        </div>
        <div className="mem-setting">
          <div>
            <div className="mem-setting-name">{t('asst.mem.jobs')}</div>
            <div className="muted asst-small">{t('asst.mem.jobs_hint')}</div>
            <div className="muted asst-small">
              {j.reflected ? t('asst.mem.reflected', { when: relativeTime(t, j.reflected), note: jobText(t, j.reflect_result) }) : t('asst.mem.reflect_never')}
              {' · '}
              {j.tidied ? t('asst.mem.tidied', { when: relativeTime(t, j.tidied), note: jobText(t, j.tidy_result) }) : t('asst.mem.tidy_never')}
            </div>
          </div>
          <Switch checked={j.enabled} onChange={(v) => set({ jobs: v })} label={t('asst.mem.jobs')} />
        </div>
        <div className="mem-setting-btns">
          <Button variant="secondary" className="btn-sm" busy={busy === 'reflect'} disabled={Boolean(busy) || j.running} onClick={() => run('reflect')}>
            {t('asst.mem.run_reflect')}
          </Button>
          <Button variant="secondary" className="btn-sm" busy={busy === 'tidy'} disabled={Boolean(busy) || j.running} onClick={() => run('tidy')}>
            {t('asst.mem.run_tidy')}
          </Button>
          <Button variant="secondary" className="btn-sm" busy={busy === 'seed'} disabled={Boolean(busy) || j.running} onClick={() => run('seed')}>
            {t('asst.mem.run_seed')}
          </Button>
        </div>
        <div className="mem-setting">
          <div>
            <div className="mem-setting-name">{t('asst.mem.index')}</div>
            <div className="muted asst-small">{t('asst.mem.index_hint', { docs: data.index?.docs ?? 0, files: data.index?.files ?? 0, imported: data.imported })}</div>
          </div>
          <Button variant="plain" className="btn-sm" busy={busy === 'index'} disabled={Boolean(busy)} onClick={() => run('index')}>
            <RefreshCw size={14} aria-hidden="true" />
            {t('asst.mem.rebuild')}
          </Button>
        </div>
        <div className="mem-setting">
          <div>
            <div className="mem-setting-name">{t('asst.mem.export')}</div>
            <div className="muted asst-small">{t('asst.mem.export_hint')}</div>
          </div>
          <Button variant="plain" className="btn-sm" busy={busy === 'export'} disabled={Boolean(busy)} onClick={exportZip}>
            <Download size={14} aria-hidden="true" />
            {t('asst.mem.export_btn')}
          </Button>
        </div>
      </div>
    </details>
  );
}
