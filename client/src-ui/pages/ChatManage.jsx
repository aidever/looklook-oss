// 对话页里管终端 TUI 才能管的东西（docs/CHAT_TUI_PARITY.md）：/status、/feedback、插件、子代理、hook 添加、沙箱、MCP 登录、
// Codex 的实验功能。ChatView 的设置栏和斜杠命令用这些。
import { useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Blocks, Bot, ExternalLink, LoaderCircle, Plus, Search, ShieldCheck, Trash2, Wrench, X } from 'lucide-react';
import { api } from '../shared/api.js';
import { errorText } from '../shared/i18n.js';
import { Button, Modal, useConfirm, useToast } from '../shared/ui.jsx';

const useT = useTranslation;

/** 发请求，出错弹提示；返回结果或 null */
function useRun() {
  const { t } = useT();
  const toast = useToast();
  const [busy, setBusy] = useState(null);
  const run = async (key, fn) => {
    setBusy(key);
    try {
      return (await fn()) ?? true;
    } catch (e) {
      toast(errorText(t, e), 'error');
      return null;
    } finally {
      setBusy(null);
    }
  };
  return [busy, run];
}

// ---------------- /status ----------------

/** /status：Claude Code / Codex 自己的会话信息，分节的键值 */
export function StatusDialog({ id, onClose }) {
  const { t } = useT();
  const [data, setData] = useState(null);
  const [error, setError] = useState(null);
  useEffect(() => {
    api.get(`/instances/${id}/chat/status`).then(setData, setError);
  }, [id]);
  return (
    <Modal open onClose={onClose} title={t('chat.status_title')} wide>
      {error ? (
        <p className="chat-note">{errorText(t, error)}</p>
      ) : !data ? (
        <LoaderCircle size={18} className="spin" />
      ) : (
        <div className="chat-status">
          {(data.sections ?? []).map((s) =>
            s.rows?.length ? (
              <section key={s.title}>
                <h4 className="chat-side-title">{s.title}</h4>
                <dl>
                  {s.rows.map((r, n) => (
                    // eslint-disable-next-line react/no-array-index-key -- 标签可能重复
                    <div key={n}>
                      <dt>{r.label}</dt>
                      <dd>{String(r.value ?? '')}</dd>
                    </div>
                  ))}
                </dl>
              </section>
            ) : null,
          )}
        </div>
      )}
      <div className="modal-actions">
        <Button variant="secondary" onClick={onClose}>
          {t('action.close')}
        </Button>
      </div>
    </Modal>
  );
}

// ---------------- /feedback ----------------

export function FeedbackDialog({ id, agent, onClose }) {
  const { t } = useT();
  const toast = useToast();
  const [text, setText] = useState('');
  const [attach, setAttach] = useState(false);
  const [busy, run] = useRun();
  const send = async () => {
    const r = await run('send', () => api.post(`/instances/${id}/chat/feedback`, { text, transcript: attach }));
    if (r) {
      toast(t('chat.feedback_sent'), 'ok');
      onClose();
    }
  };
  return (
    <Modal open onClose={onClose} title={t('chat.feedback_title', { agent })}>
      <p className="chat-note">{t('chat.feedback_note', { agent })}</p>
      <textarea className="chat-field chat-feedback-text" rows={5} value={text} onChange={(e) => setText(e.target.value)} placeholder={t('chat.feedback_ph')} />
      <label className="chat-q-multi">
        <input type="checkbox" checked={attach} onChange={(e) => setAttach(e.target.checked)} />
        {t(agent === 'Codex' ? 'chat.feedback_logs' : 'chat.feedback_transcript')}
      </label>
      <div className="modal-actions">
        <Button variant="secondary" onClick={onClose}>
          {t('action.cancel')}
        </Button>
        <Button busy={busy === 'send'} disabled={!text.trim() || !!busy} onClick={send}>
          {t('chat.feedback_send')}
        </Button>
      </div>
    </Modal>
  );
}

// ---------------- 插件 ----------------

/** 设置栏里的“插件”：装了的（开关、更新、卸载），浏览市场装新的，加减市场 */
export function PluginsSection({ id, agent }) {
  const { t } = useT();
  const confirm = useConfirm();
  const [rev, setRev] = useState(0);
  const [data, setData] = useState(null);
  const [error, setError] = useState(null);
  const [browse, setBrowse] = useState(false);
  const [busy, run] = useRun();
  useEffect(() => {
    let alive = true;
    setError(null);
    api.get(`/instances/${id}/chat/plugins`).then(
      (d) => alive && setData(d),
      (e) => alive && setError(e),
    );
    return () => {
      alive = false;
    };
  }, [id, rev]);
  const op = async (key, body) => {
    const r = await run(key, () => api.post(`/instances/${id}/chat/plugins`, body));
    // 要运行市场声明的命令：给用户看清楚再装
    if (r?.confirm) {
      const ok = await confirm({ title: t('chat.plugin_cmd_title'), body: r.confirm.command ?? '', confirm: t('chat.plugin_cmd_ok'), danger: true });
      if (ok) await run(key, () => api.post(`/instances/${id}/chat/plugins`, { ...body, accept: r.confirm.sha256 }));
    }
    setRev((x) => x + 1);
    return r;
  };
  const installed = data?.installed ?? [];
  return (
    <>
      <h4 className="chat-side-title">
        <Blocks size={15} aria-hidden="true" /> {t('chat.plugins')}
        <button type="button" className="chat-more" onClick={() => setBrowse(true)} disabled={!data}>
          <Plus size={13} aria-hidden="true" /> {t('chat.plugins_browse')}
        </button>
      </h4>
      {error ? (
        <p className="chat-note">{errorText(t, error)}</p>
      ) : !data ? (
        <LoaderCircle size={16} className="spin" />
      ) : installed.length === 0 ? (
        <p className="chat-note">{t('chat.plugins_empty')}</p>
      ) : (
        <ul className="chat-rules">
          {installed.map((p) => {
            const key = `${p.id}:${p.scope ?? ''}`;
            const scope = agent === 'claude' ? p.scope : undefined;
            return (
              <li key={key} data-off={p.enabled === false ? '' : undefined}>
                <b className="ellipsis" title={p.description ?? p.id}>
                  {p.name ?? p.id}
                </b>
                {p.version && <span className="chat-note">{p.version}</span>}
                {scope && <span className="chat-note">{t(`chat.src_${scope}`, { defaultValue: scope })}</span>}
                <button type="button" className="chat-more" disabled={!!busy} onClick={() => op(`t:${key}`, { op: p.enabled === false ? 'enable' : 'disable', name: p.id, scope })}>
                  {busy === `t:${key}` ? <LoaderCircle size={13} className="spin" /> : t(p.enabled === false ? 'chat.mcp_enable' : 'chat.mcp_disable')}
                </button>
                {agent === 'claude' && (
                  <button type="button" className="chat-more" disabled={!!busy} onClick={() => op(`u:${key}`, { op: 'update', name: p.id, scope })}>
                    {busy === `u:${key}` ? <LoaderCircle size={13} className="spin" /> : t('chat.plugin_update')}
                  </button>
                )}
                <button
                  type="button"
                  className="chat-more"
                  disabled={!!busy}
                  aria-label={t('chat.plugin_uninstall')}
                  title={t('chat.plugin_uninstall')}
                  onClick={async () => {
                    if (await confirm({ title: t('chat.plugin_uninstall_q', { name: p.name ?? p.id }), confirm: t('chat.plugin_uninstall'), danger: true })) op(`x:${key}`, { op: 'uninstall', name: p.id, scope });
                  }}
                >
                  {busy === `x:${key}` ? <LoaderCircle size={13} className="spin" /> : <Trash2 size={13} aria-hidden="true" />}
                </button>
              </li>
            );
          })}
        </ul>
      )}
      {browse && data && <PluginBrowser agent={agent} data={data} busy={busy} onOp={op} onClose={() => setBrowse(false)} />}
    </>
  );
}

function PluginBrowser({ agent, data, busy, onOp, onClose }) {
  const { t } = useT();
  const confirm = useConfirm();
  const [q, setQ] = useState('');
  const [scope, setScope] = useState('user');
  const [market, setMarket] = useState('');
  const have = useMemo(() => new Set((data.installed ?? []).map((p) => p.id)), [data]);
  const list = useMemo(() => {
    const s = q.trim().toLowerCase();
    return (data.available ?? []).filter((p) => !have.has(p.id) && (!s || `${p.name} ${p.id} ${p.description ?? ''}`.toLowerCase().includes(s))).slice(0, 200);
  }, [data, q, have]);
  return (
    <Modal open onClose={onClose} title={t('chat.plugins_browse')} wide className="chat-plugins-modal">
      <div className="chat-acts">
        <Search size={15} aria-hidden="true" />
        <input className="chat-field grow" placeholder={t('chat.plugins_search')} value={q} onChange={(e) => setQ(e.target.value)} />
        {agent === 'claude' && (
          <select className="chat-field" value={scope} onChange={(e) => setScope(e.target.value)} aria-label={t('chat.config_rule_where')}>
            {['user', 'project', 'local'].map((k) => (
              <option key={k} value={k}>
                {t(`chat.src_${k}`)}
              </option>
            ))}
          </select>
        )}
      </div>
      <ul className="chat-rules chat-plugin-list">
        {list.length === 0 && <li className="chat-note">{t('chat.plugins_none')}</li>}
        {list.map((p) => (
          <li key={p.id}>
            <span className="chat-plugin-text">
              <b>{p.name ?? p.id}</b>
              <span className="chat-note">{p.market}</span>
              {p.description && <small>{p.description}</small>}
            </span>
            <Button variant="secondary" busy={busy === `i:${p.id}`} disabled={!!busy || p.available === false} onClick={() => onOp(`i:${p.id}`, { op: 'install', name: p.id, scope: agent === 'claude' ? scope : undefined })}>
              {t('chat.plugin_install')}
            </Button>
          </li>
        ))}
      </ul>
      <h4 className="chat-side-title">{t('chat.plugin_markets')}</h4>
      <ul className="chat-rules">
        {(data.markets ?? []).map((m) => (
          <li key={m.name}>
            <b className="ellipsis">{m.name}</b>
            {m.source && m.source !== m.name && <span className="chat-note ellipsis">{m.source}</span>}
            <button type="button" className="chat-more" disabled={!!busy} onClick={() => onOp(`mu:${m.name}`, { op: 'market_update', name: m.name })}>
              {busy === `mu:${m.name}` ? <LoaderCircle size={13} className="spin" /> : t('chat.plugin_update')}
            </button>
            <button
              type="button"
              className="chat-more"
              disabled={!!busy}
              aria-label={t('chat.config_remove')}
              onClick={async () => {
                if (await confirm({ title: t('chat.plugin_market_remove_q', { name: m.name }), confirm: t('chat.config_remove'), danger: true })) onOp(`mr:${m.name}`, { op: 'market_remove', name: m.name });
              }}
            >
              <X size={13} aria-hidden="true" />
            </button>
          </li>
        ))}
      </ul>
      <form
        className="chat-acts chat-config-add"
        onSubmit={async (e) => {
          e.preventDefault();
          if (market.trim() && (await onOp('ma', { op: 'market_add', name: market.trim() }))) setMarket('');
        }}
      >
        <input className="chat-field grow" placeholder={t('chat.plugin_market_ph')} value={market} onChange={(e) => setMarket(e.target.value)} />
        <Button type="submit" variant="secondary" busy={busy === 'ma'} disabled={!!busy || !market.trim()}>
          {t('chat.config_add')}
        </Button>
      </form>
      <div className="modal-actions">
        <Button variant="secondary" onClick={onClose}>
          {t('action.close')}
        </Button>
      </div>
    </Modal>
  );
}

// ---------------- Claude：子代理 ----------------

/** 子代理（/agents）：列出项目的和用户的，新建（写模板后在文件查看器里打开改）、打开、删除 */
export function AgentsSection({ id, openFile, fileEp }) {
  const { t } = useT();
  const confirm = useConfirm();
  const [rev, setRev] = useState(0);
  const [data, setData] = useState(null);
  const [form, setForm] = useState(null);
  const [busy, run] = useRun();
  useEffect(() => {
    api.get(`/instances/${id}/chat/agents`).then(setData, () => setData({ items: [] }));
  }, [id, rev]);
  const open = (path) => openFile?.({ ep: fileEp, path });
  const create = async () => {
    const r = await run('create', () => api.post(`/instances/${id}/chat/agents`, { op: 'create', ...form }));
    if (r?.path) {
      setForm(null);
      setRev((x) => x + 1);
      open(r.path);
    }
  };
  return (
    <>
      <h4 className="chat-side-title">
        <Bot size={15} aria-hidden="true" /> {t('chat.subagents')}
        <button type="button" className="chat-more" onClick={() => setForm(form ? null : { source: 'project', name: '', description: '', prompt: '' })}>
          <Plus size={13} aria-hidden="true" /> {t('chat.subagent_new')}
        </button>
      </h4>
      {form && (
        <form
          className="chat-subagent-form"
          onSubmit={(e) => {
            e.preventDefault();
            if (form.name.trim()) create();
          }}
        >
          <div className="chat-acts">
            <input className="chat-field grow" placeholder={t('chat.subagent_name_ph')} value={form.name} onChange={(e) => setForm({ ...form, name: e.target.value.replace(/[^A-Za-z0-9_-]/g, '-') })} />
            <select className="chat-field" value={form.source} onChange={(e) => setForm({ ...form, source: e.target.value })} aria-label={t('chat.config_rule_where')}>
              {['project', 'user'].map((k) => (
                <option key={k} value={k}>
                  {t(`chat.src_${k}`)}
                </option>
              ))}
            </select>
          </div>
          <input className="chat-field" placeholder={t('chat.subagent_desc_ph')} value={form.description} onChange={(e) => setForm({ ...form, description: e.target.value })} />
          <textarea className="chat-field" rows={4} placeholder={t('chat.subagent_prompt_ph')} value={form.prompt} onChange={(e) => setForm({ ...form, prompt: e.target.value })} />
          <div className="chat-acts">
            <Button type="submit" busy={busy === 'create'} disabled={!!busy || !form.name.trim()}>
              {t('chat.subagent_create')}
            </Button>
            <Button variant="ghost" onClick={() => setForm(null)}>
              {t('action.cancel')}
            </Button>
          </div>
        </form>
      )}
      {!data ? (
        <LoaderCircle size={16} className="spin" />
      ) : data.items.length === 0 ? (
        <p className="chat-note">{t('chat.subagents_empty')}</p>
      ) : (
        <ul className="chat-rules">
          {data.items.map((a) => (
            <li key={a.path}>
              <b className="ellipsis" title={a.description ?? a.name}>
                {a.name}
              </b>
              <span className="chat-note">{t(`chat.src_${a.source}`)}</span>
              {openFile && (
                <button type="button" className="chat-more" onClick={() => open(a.path)}>
                  {t('chat.config_open')}
                </button>
              )}
              <button
                type="button"
                className="chat-more"
                disabled={!!busy}
                aria-label={t('chat.config_remove')}
                title={t('chat.config_remove')}
                onClick={async () => {
                  if (!(await confirm({ title: t('chat.subagent_delete_q', { name: a.name }), confirm: t('chat.config_remove'), danger: true }))) return;
                  if (await run(`d:${a.path}`, () => api.post(`/instances/${id}/chat/agents`, { op: 'delete', path: a.path }))) setRev((x) => x + 1);
                }}
              >
                <X size={13} aria-hidden="true" />
              </button>
            </li>
          ))}
        </ul>
      )}
    </>
  );
}

// ---------------- Claude：加 hook、沙箱 ----------------

/** 加一个命令 hook：事件（Claude Code 给的目录）、matcher（支持的事件才有）、命令、存到哪 */
export function HookAdd({ id, events, onDone }) {
  const { t } = useT();
  const [busy, run] = useRun();
  const list = events?.length ? events : [{ name: 'PreToolUse', supportsMatcher: true }, { name: 'PostToolUse', supportsMatcher: true }, { name: 'Stop', supportsMatcher: false }];
  const [f, setF] = useState({ event: list[0].name, matcher: '', command: '', source: 'local' });
  const ev = list.find((e) => e.name === f.event);
  return (
    <form
      className="chat-subagent-form"
      onSubmit={async (e) => {
        e.preventDefault();
        if (!f.command.trim()) return;
        if (await run('add', () => api.post(`/instances/${id}/chat/config/hook`, { ...f, matcher: ev?.supportsMatcher ? f.matcher.trim() : '', command: f.command.trim(), add: true }))) {
          setF({ ...f, matcher: '', command: '' });
          onDone();
        }
      }}
    >
      <div className="chat-acts">
        <select className="chat-field grow" value={f.event} onChange={(e) => setF({ ...f, event: e.target.value })} aria-label={t('chat.hook_event_label')} title={ev?.summary}>
          {list.map((e) => (
            <option key={e.name} value={e.name}>
              {e.name}
            </option>
          ))}
        </select>
        <select className="chat-field" value={f.source} onChange={(e) => setF({ ...f, source: e.target.value })} aria-label={t('chat.config_rule_where')}>
          {['local', 'project', 'user'].map((k) => (
            <option key={k} value={k}>
              {t(`chat.src_${k}`)}
            </option>
          ))}
        </select>
      </div>
      {ev?.summary && <p className="chat-note">{ev.summary}</p>}
      {ev?.supportsMatcher && <input className="chat-field" placeholder={t('chat.hook_matcher_ph')} value={f.matcher} onChange={(e) => setF({ ...f, matcher: e.target.value })} />}
      <div className="chat-acts">
        <input className="chat-field grow" placeholder={t('chat.hook_command_ph')} value={f.command} onChange={(e) => setF({ ...f, command: e.target.value })} />
        <Button type="submit" variant="secondary" busy={busy === 'add'} disabled={!!busy || !f.command.trim()}>
          {t('chat.config_add')}
        </Button>
      </div>
    </form>
  );
}

/** 沙箱（/sandbox）：命令在隔离里跑。开关写本机 settings */
export function SandboxRow({ id, sandbox, onDone }) {
  const { t } = useT();
  const [busy, run] = useRun();
  if (!sandbox?.supported) return null;
  return (
    <>
      <h4 className="chat-side-title">
        <ShieldCheck size={15} aria-hidden="true" /> {t('chat.sandbox')}
      </h4>
      <label className="chat-q-multi">
        <input
          type="checkbox"
          checked={!!sandbox.enabled}
          disabled={!!busy || sandbox.locked}
          onChange={async (e) => {
            const on = e.target.checked;
            if (await run('sb', () => api.post(`/instances/${id}/chat/config/sandbox`, { on }))) onDone();
          }}
        />
        {t('chat.sandbox_on')}
      </label>
      {sandbox.errors?.length > 0 && <p className="chat-note">{t('chat.sandbox_missing', { what: sandbox.errors.join('; ') })}</p>}
    </>
  );
}

// ---------------- MCP 登录 ----------------

/**
 * MCP 服务器的 OAuth 登录：打开授权页；授权完浏览器会跳到这台电脑的 localhost 回调。在这台电脑上用浏览器时自动完成；
 * 远程（手机、别的电脑）那个地址打不开，把地址栏里的网址贴回来交给看看。
 * `start` 返回授权网址；`submit(url)` 交回跳转网址
 */
export function McpLogin({ name, start, submit, onDone, onCancel }) {
  const { t } = useT();
  const [busy, run] = useRun();
  const [url, setUrl] = useState(null);
  const [back, setBack] = useState('');
  useEffect(() => {
    let alive = true;
    run('start', start).then((r) => {
      if (!alive) return;
      if (r?.url) {
        setUrl(r.url);
        window.open(r.url, '_blank', 'noopener');
      } else onCancel();
    });
    return () => {
      alive = false;
    };
  }, [name]);
  return (
    <div className="chat-mcp-login">
      {!url ? (
        <LoaderCircle size={14} className="spin" />
      ) : (
        <>
          <p className="chat-note">
            {t('chat.mcp_login_opened')}{' '}
            <a href={url} target="_blank" rel="noreferrer noopener">
              <ExternalLink size={12} aria-hidden="true" /> {t('chat.mcp_login_again')}
            </a>
          </p>
          <form
            className="chat-acts"
            onSubmit={async (e) => {
              e.preventDefault();
              if (back.trim() && (await run('back', () => submit(back.trim())))) onDone();
            }}
          >
            <input className="chat-field grow" placeholder={t('chat.mcp_login_paste')} value={back} onChange={(e) => setBack(e.target.value)} />
            <Button type="submit" variant="secondary" busy={busy === 'back'} disabled={!!busy || !back.trim()}>
              {t('chat.mcp_login_finish')}
            </Button>
            <Button variant="ghost" onClick={onDone}>
              {t('chat.mcp_login_done')}
            </Button>
          </form>
        </>
      )}
    </div>
  );
}

// ---------------- Codex：hook、实验功能 ----------------

export function CodexExtras({ id }) {
  const { t } = useT();
  const [rev, setRev] = useState(0);
  const [data, setData] = useState(null);
  const [busy, run] = useRun();
  useEffect(() => {
    api.get(`/instances/${id}/chat/codex-extras`).then(setData, () => setData({ hooks: [], features: [] }));
  }, [id, rev]);
  if (!data) return <LoaderCircle size={16} className="spin" />;
  return (
    <>
      <h4 className="chat-side-title">
        <Wrench size={15} aria-hidden="true" /> {t('chat.config_hooks')}
      </h4>
      {data.hooks.length === 0 ? (
        <p className="chat-note">{t('chat.config_hooks_empty')}</p>
      ) : (
        <ul className="chat-rules">
          {data.hooks.map((h, n) => (
            // eslint-disable-next-line react/no-array-index-key -- 可能一样
            <li key={n} data-off={h.enabled === false || h.trust === 'untrusted' ? '' : undefined}>
              <span className="badge">{h.event}</span>
              <code className="ellipsis" title={h.command}>
                {h.matcher ? `[${h.matcher}] ` : ''}
                {h.command}
              </code>
              <span className="chat-note">{t(`chat.hook_trust_${h.trust}`, { defaultValue: h.trust ?? '' })}</span>
            </li>
          ))}
        </ul>
      )}
      <p className="chat-note">{t('chat.codex_hooks_note')}</p>
      {data.features.length > 0 && (
        <>
          <h4 className="chat-side-title">{t('chat.codex_features')}</h4>
          <ul className="chat-rules">
            {data.features.map((f) => (
              <li key={f.name}>
                <label className="chat-q-multi chat-feature">
                  <input
                    type="checkbox"
                    checked={!!f.enabled}
                    disabled={!!busy}
                    onChange={async (e) => {
                      const on = e.target.checked;
                      if (await run(f.name, () => api.post(`/instances/${id}/chat/codex-feature`, { name: f.name, on }))) setRev((x) => x + 1);
                    }}
                  />
                  <span>
                    <b>{f.title}</b>
                    {f.description && <small>{f.description}</small>}
                  </span>
                </label>
              </li>
            ))}
          </ul>
        </>
      )}
    </>
  );
}
