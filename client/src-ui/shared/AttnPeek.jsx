// 会话在等你的那件事（审批、提问、选择框、额度用完），就地处理：任务看板的展开、助理页的活动面板和对话上方的提醒条共用。
// 数据是会话简报里的 attn（chat/mod.rs brief），操作走对话界面的接口（按终端自动发给它所在的电脑）。
import { AlertTriangle, Check } from 'lucide-react';
import { useState } from 'react';
import { useTranslation } from 'react-i18next';

import { api } from './api.js';
import { limitAt } from './board.js';
import { errorText } from './i18n.js';
import GuardNote from './GuardNote.jsx';
import { Button, Spinner, useConfirm, useToast } from './ui.jsx';
import { useSubmitLock } from './submitLock.js';

// 额度菜单里“等重置后自动继续”（Claude Code 的原文，同客户端 screen.rs 的 LIMIT_AUTO）
const LIMIT_AUTO = 'Wait here, then continue automatically';
// 能答“本次会话都允许”的（DSH 只能允许 / 拒绝）
const SESSION_ALLOW = ['claude', 'codex'];

/**
 * `id`：会话；`attn`：它在等的事；`agent`：显示名（Claude / Codex）；`launch`：claude / codex / dsh；`limit`：额度状态；
 * `open`：打开会话的按钮（放在操作那一行最后）；`compact`：窄的地方（助理页右栏、提醒条）用小一号的
 */
export default function AttnPeek({ id, attn: a, agent, launch, limit, open = null, compact = false, className = '' }) {
  const { t } = useTranslation();
  const toast = useToast();
  const confirm = useConfirm();
  const [other, setOther] = useState('');
  // 点一下就锁住，直到这件事换了（列表几秒才取一次，请求早就回来了，这段时间不锁着会被连点、看着也像没点上）。
  // 已经在别处处理过了（CHAT_STALE）也当送到：等下一次刷新自然消失
  const lock = useSubmitLock(`${id}|${a.kind ?? ''}|${a.pending_id ?? ''}|${a.title ?? ''}|${a.detail ?? ''}|${(a.options ?? []).join('|')}`);
  const busy = lock.what ?? '';
  // 答完马上告诉页面去刷新（助理页几秒才取一次，不刷的话卡片一直挂着，看着像没送到）
  const answered = () => window.dispatchEvent(new CustomEvent('ll:attn-answered', { detail: { id } }));
  const run = (key, fn) =>
    lock
      .run(key, fn)
      .then(answered, (e) => toast(errorText(t, e), 'error'));
  const answer = (decision, extra = {}) => run(decision, () => api.post(`/instances/${id}/chat/permission`, { id: a.pending_id, decision, ...extra }));
  const risky = a.risk === 'high' || Boolean(a.guard);
  const allow = async () => {
    if (risky && !(await confirm({ title: t('board.confirm_high_title'), body: a.detail, confirm: t('board.allow'), danger: true }))) return;
    answer('allow');
  };
  const reply = (label) => run(`a:${label}`, () => api.post(`/instances/${id}/chat/permission`, { id: a.pending_id, decision: 'answer', answers: { [a.question ?? a.detail]: label } }));
  const choose = (index) => run(`d${index}`, () => api.post(`/instances/${id}/chat/dialog`, { index, title: a.title ?? '', label: a.options[index] }));

  let body = null;
  let actions = open;
  if (a.kind === 'permission') {
    body = (
      <>
        <p className="bd-peek-lead">{t('board.wants', { agent, tool: a.tool })}</p>
        {a.detail && <pre className="bd-code">{a.detail}</pre>}
        {a.guard ? (
          <GuardNote from={a.guard_from} rule={a.guard} className="bd-warn" />
        ) : (
          a.risk === 'high' && (
            <p className="bd-warn">
              <AlertTriangle size={16} aria-hidden="true" />
              {t('board.risk_high_note')}
            </p>
          )
        )}
      </>
    );
    const off = Boolean(busy) || a.pending_id == null;
    actions = (
      <>
        <Button busy={busy === 'allow'} disabled={off} onClick={allow}>
          {a.guard ? t('chat.guard_allow') : t('board.allow')}
        </Button>
        {/* 同一类操作这个会话里不再问：派出去的活一步步点“允许”太累。危险的、命令规则拦下的只能一次次批 */}
        {!risky && SESSION_ALLOW.includes(launch) && a.tool !== 'permissions' && (
          <Button variant="secondary" busy={busy === 'allow_session'} disabled={off} onClick={() => answer('allow_session')} title={t('chat.allow_session_hint')}>
            {t(compact ? 'board.allow_session_short' : 'chat.allow_session')}
          </Button>
        )}
        <Button variant="ghost" className="bd-deny" busy={busy === 'deny'} disabled={off} onClick={() => answer('deny')}>
          {t('board.deny')}
        </Button>
        {open}
      </>
    );
  } else if (a.kind === 'question') {
    body = (
      <>
        <p className="bd-peek-lead">{a.detail}</p>
        {a.options?.length > 0 && (
          <div className="bd-options">
            {a.options.map((o) => (
              <button key={o} type="button" className="bd-option" disabled={Boolean(busy)} aria-busy={busy === `a:${o}` || undefined} onClick={() => reply(o)}>
                {busy === `a:${o}` && <Spinner size={14} />}
                {o}
              </button>
            ))}
          </div>
        )}
        {a.options?.length > 0 && (
          <form
            className="bd-other"
            onSubmit={(e) => {
              e.preventDefault();
              if (!busy && other.trim()) reply(other.trim());
            }}
          >
            <input value={other} onChange={(e) => setOther(e.target.value)} placeholder={t('board.other')} aria-label={t('board.other')} />
            <Button variant="ghost" type="submit" busy={busy === `a:${other.trim()}`} disabled={!other.trim() || Boolean(busy)}>
              {t('board.send')}
            </Button>
          </form>
        )}
      </>
    );
  } else if (a.kind === 'limit') {
    const auto = (a.options ?? []).findIndex((o) => o.startsWith(LIMIT_AUTO));
    body = <p className="bd-peek-lead">{limitAt(limit) ? t('board.limit_waiting', { at: limitAt(limit) }) : t('board.limit_body')}</p>;
    actions = (
      <>
        {auto >= 0 && (
          <Button busy={busy === `d${auto}`} disabled={Boolean(busy)} onClick={() => choose(auto)}>
            {t('board.limit_wait')}
          </Button>
        )}
        {open}
      </>
    );
  } else if (a.kind === 'dialog') {
    const opts = a.options ?? [];
    body = (
      <>
        <p className="bd-peek-lead">{a.title || t('board.dialog_unknown')}</p>
        {opts.length > 0 && opts.length <= 4 && (
          <div className="bd-options">
            {opts.map((o, n) => (
              <button key={o} type="button" className="bd-option" disabled={Boolean(busy)} aria-busy={busy === `d${n}` || undefined} onClick={() => choose(n)}>
                {busy === `d${n}` && <Spinner size={14} />}
                {o}
              </button>
            ))}
          </div>
        )}
      </>
    );
  } else {
    body = <p className="bd-peek-lead">{a.detail ? t('board.plan', { p: a.detail }) : t('board.dialog_unknown')}</p>;
  }
  return (
    <div className={`bd-peek ${compact ? 'bd-peek-compact' : ''} ${className}`} aria-busy={Boolean(busy) || undefined}>
      {body}
      <div className="bd-actions">
        {actions}
        {lock.sent && (
          <span className="bd-sent" role="status">
            <Check size={14} aria-hidden="true" /> {t('board.sent')}
          </span>
        )}
      </div>
    </div>
  );
}
