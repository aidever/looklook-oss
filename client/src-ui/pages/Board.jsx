// 任务看板（docs/TASK_BOARD.md §5，样机 A）：所有电脑上的 AI 会话按“等你处理 → 正在干活 → 刚结束”分组，
// 一行一句话说清现状。点一行展开（宽屏在右侧详情栏），能直接允许 / 拒绝、回答选项、处理选择框、停止、知道了。
// 数据就是会话列表（每 5 秒取一次，sessions.jsx），操作复用对话界面的接口（按终端自动发给它所在的电脑）。
import { Check, ChevronRight, ExternalLink, Inbox, Plus, X } from 'lucide-react';
import { useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';

import { api, terminalDevice } from '../shared/api.js';
import { awayRecap, boardSort, boardSpan, boardState, boardSummary, useAwaySince, BOARD_GROUPS } from '../shared/board.js';
import { DeviceTag, SessionIcon } from '../shared/brands.jsx';
import { useDevices } from '../shared/devices.jsx';
import { errorText } from '../shared/i18n.js';
import { baseName, useSessions } from '../shared/sessions.jsx';
import { spinnerText } from '../shared/spinner.js';
import { spanText } from '../shared/time.js';
import { Link, useRouter } from '../shared/router.jsx';
import AttnPeek from '../shared/AttnPeek.jsx';
import { Button, Empty, Note, Skeleton, useConfirm, useToast } from '../shared/ui.jsx';
import { useSubmitLock } from '../shared/submitLock.js';
import { PageHead } from '../shared/page.jsx';
import { useSessionUi } from './Sessions.jsx';
import { useNarrow } from './TerminalView.jsx';

const AGENT_NAMES = { claude: 'Claude', codex: 'Codex' };

/** 飞书通知配好了但这台电脑没连上（docs/TASK_BOARD.md §6.6）：看板顶部提示一行。旧版客户端没有这个接口时不提示。 */
function useFeishuDown() {
  const [down, setDown] = useState(false);
  useEffect(() => {
    let alive = true;
    const load = () =>
      api.get('/notify').then(
        (d) => alive && setDown(Boolean(d.feishu?.bound && !d.status?.connected && d.status?.error)),
        () => alive && setDown(false),
      );
    load();
    const id = setInterval(load, 60000);
    return () => {
      alive = false;
      clearInterval(id);
    };
  }, []);
  return down;
}

/** 每 30 秒重画一次（“等了 3 分钟”） */
function useNow() {
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    const id = setInterval(() => setNow(Date.now()), 30000);
    return () => clearInterval(id);
  }, []);
  return now;
}

function useActivityText() {
  const { t, i18n } = useTranslation();
  return (activity, tool) => {
    const s = (activity ?? '').trim();
    if (!s) return '';
    if (s === 'compacting') return t('chat.compacting');
    if (s === 'thinking') return t('chat.codex_thinking');
    if (s === 'waiting_agents') return t('chat.codex_waiting_agents');
    if (tool === 'Bash') return t('board.do_run', { detail: s });
    return spinnerText(s, i18n.language).text;
  };
}

/** 会话所在的电脑（只有一台时不显示） */
function useDeviceOf() {
  const devices = useDevices();
  return (i) => (devices.multi ? devices.byId?.(terminalDevice(i.id)) ?? null : null);
}

function StateIcon({ state, outcome }) {
  if (state === 'attn') return <span className="bd-dot bd-dot-attn" aria-hidden="true" />;
  if (state === 'working') return <span className="bd-dot bd-dot-working" aria-hidden="true" />;
  if (outcome === 'failed')
    return (
      <span className="bd-end bd-end-fail" aria-hidden="true">
        <X size={14} strokeWidth={2.6} />
      </span>
    );
  return (
    <span className="bd-end bd-end-ok" aria-hidden="true">
      <Check size={14} strokeWidth={2.6} />
    </span>
  );
}

function Row({ i, state, selected, onSelect, now, act }) {
  const { t } = useTranslation();
  const deviceOf = useDeviceOf();
  const dev = deviceOf(i);
  const c = i.chat ?? {};
  const sum = boardSummary(t, i, act);
  const span = boardSpan(i, now);
  const title = c.title || i.name || baseName(i.workdir);
  const risk = state === 'attn' && (c.attn?.risk === 'high' || Boolean(c.attn?.guard));
  return (
    <button type="button" className={`bd-row bd-${state} ${selected ? 'selected' : ''}`} onClick={onSelect} aria-expanded={selected}>
      <StateIcon state={state} outcome={c.turn?.outcome} />
      <SessionIcon inst={i} size={18} className="bd-agent" />
      <span className="bd-text">
        <span className="bd-title-line">
          <b className="ellipsis">{title}</b>
          {span != null && <span className="bd-span">{t(`board.span_${state}`, { span: spanText(t, span) })}</span>}
        </span>
        <span className={`bd-sum ${sum.code ? 'mono' : ''}`} data-kind={sum.kind}>
          {risk && <span className="bd-risk">{t('board.risk_high')}</span>}
          <span className="ellipsis">{sum.text}</span>
        </span>
        <span className="bd-meta">
          <span className="ellipsis">{baseName(i.workdir) || i.workdir}</span>
          {dev && <DeviceTag device={dev} />}
        </span>
      </span>
      <ChevronRight className="bd-chev" size={16} aria-hidden="true" />
    </button>
  );
}

/** 展开后的内容：按状态给能直接做的事 */
function Peek({ i, state, onOpen }) {
  const { t } = useTranslation();
  const toast = useToast();
  const confirm = useConfirm();
  const c = i.chat ?? {};
  const a = c.attn ?? {};
  const agent = AGENT_NAMES[c.agent ?? i.launch] ?? i.launch;
  // 点一下就锁住，直到这一项的状态变了（会话列表每 5 秒才取一次，请求早就回来了，这段时间不锁着会被连点、看着也像没点上）。
  // 已经在别处处理过了（CHAT_STALE）也当送到：等下一次刷新自然消失
  const lock = useSubmitLock(`${i.id}|${state}|${a.kind ?? ''}|${a.pending_id ?? ''}|${a.title ?? ''}|${a.detail ?? ''}|${(a.options ?? []).join('|')}`);
  const busy = lock.what ?? '';
  const run = (key, fn) => lock.run(key, fn).catch((e) => toast(errorText(t, e), 'error'));
  const seen = () => run('seen', () => api.post(`/instances/${i.id}/chat/seen`, {}));
  const stop = async () => {
    if (!(await confirm({ title: t('board.stop_title', { agent }), body: t('board.stop_body'), confirm: t('board.stop'), danger: true }))) return;
    run('stop', () => api.post(`/instances/${i.id}/chat/interrupt`));
  };
  const open = (
    <Button variant="ghost" icon={ExternalLink} onClick={onOpen}>
      {t('board.open')}
    </Button>
  );

  if (state === 'attn') return <AttnPeek id={i.id} attn={a} agent={agent} launch={c.agent ?? i.launch} limit={c.limit} open={open} />;
  let body = null;
  let actions = open;
  if (state === 'working') {
    actions = (
      <>
        {open}
        <Button variant="ghost" className="bd-deny" busy={busy === 'stop'} disabled={Boolean(busy)} onClick={stop}>
          {t('board.stop')}
        </Button>
      </>
    );
  } else if (state === 'ended') {
    const turn = c.turn ?? {};
    body = turn.outcome === 'failed' ? <p className="bd-peek-lead bd-error">{turn.error || t('board.failed', { error: '' })}</p> : turn.summary ? <p className="bd-peek-lead">{turn.summary}</p> : null;
    actions = (
      <>
        {open}
        <Button variant="ghost" busy={busy === 'seen'} disabled={Boolean(busy)} onClick={seen}>
          {t('board.seen')}
        </Button>
      </>
    );
  }
  return (
    <div className="bd-peek" aria-busy={Boolean(busy) || undefined}>
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

export default function Board() {
  const { t } = useTranslation();
  const sessions = useSessions();
  const devices = useDevices();
  const ui = useSessionUi();
  const { navigate } = useRouter();
  const narrow = useNarrow('(max-width: 1099px)');
  const now = useNow();
  const act = useActivityText();
  const [pick, setPick] = useState(null);
  const [showRest, setShowRest] = useState(false);
  const feishuDown = useFeishuDown();
  const [awaySince, dismissAway] = useAwaySince();

  const groups = useMemo(() => {
    const g = { attn: [], working: [], ended: [], idle: [], stopped: [] };
    for (const i of sessions.items) g[boardState(i)]?.push(i);
    for (const k of BOARD_GROUPS) g[k].sort(boardSort(k));
    return g;
  }, [sessions.items]);
  const offline = devices.multi ? devices.items.filter((d) => !d.online).length : 0;
  // 默认选中等得最久的那个；选中的不在了（处理掉了）也换成它
  const all = BOARD_GROUPS.flatMap((k) => groups[k]);
  const selected = all.find((i) => i.id === pick) ?? (narrow ? null : groups.attn[0] ?? all[0] ?? null);
  const selState = selected ? boardState(selected) : null;
  const openSession = (id) => ui.openSession(id);

  if (sessions.loading) {
    return (
      <div className="board stack">
        <Skeleton height={60} />
        <Skeleton height={180} />
      </div>
    );
  }
  const empty = all.length === 0;
  const counts = t('board.counts', { attn: groups.attn.length, working: groups.working.length });
  const away = awayRecap(sessions.items, awaySince, now);
  return (
    <div className="board">
      <PageHead title={t('board.title')} sub={empty ? t('board.sub') : counts} help="tasks" />
      {feishuDown && (
        <div className="bd-notify">
          <Note kind="warn">
            {t('board.feishu_down')} <Link to="/ai#notify">{t('board.feishu_down_fix')}</Link>
          </Note>
        </div>
      )}
      {away && (
        <div className="bd-away" role="status">
          <span className="grow">
            {t('board.away', { span: spanText(t, away.span) })}
            {[away.done > 0 && t('board.away_done', { n: away.done }), away.failed > 0 && t('board.away_failed', { n: away.failed }), away.attn > 0 && t('board.away_attn', { n: away.attn })].filter(Boolean).join(t('board.away_sep'))}
          </span>
          <button type="button" className="btn btn-plain btn-icon" onClick={dismissAway} aria-label={t('action.close')} title={t('action.close')}>
            <X size={16} />
          </button>
        </div>
      )}
      <div className={`bd-body ${!narrow && selected ? 'with-side' : ''}`}>
        <div className="bd-list">
          {empty && (
            <Empty icon={Inbox} title={t('board.empty_title')}>
              <p>{t('board.empty_body')}</p>
              <p className="muted small">{t('board.empty_phone')}</p>
              <Button icon={Plus} onClick={() => navigate('/new')}>
                {t('board.empty_go')}
              </Button>
            </Empty>
          )}
          {BOARD_GROUPS.map(
            (k) =>
              groups[k].length > 0 && (
                <section key={k} className={`bd-group bd-group-${k}`}>
                  <h2>
                    {t(`board.group_${k}`)} <span>{groups[k].length}</span>
                  </h2>
                  {groups[k].map((i) => {
                    const on = selected?.id === i.id;
                    return (
                      <div key={i.id} className={`bd-item ${on && narrow ? 'open' : ''}`}>
                        <Row i={i} state={k} selected={on} now={now} act={act} onSelect={() => setPick(on && narrow ? '' : i.id)} />
                        {on && narrow && <Peek i={i} state={k} onOpen={() => openSession(i.id)} />}
                      </div>
                    );
                  })}
                </section>
              ),
          )}
          {(groups.idle.length > 0 || groups.stopped.length > 0 || offline > 0) && (
            <div className="bd-rest">
              <button type="button" className="bd-chip" onClick={() => setShowRest(!showRest)} aria-expanded={showRest}>
                {[groups.idle.length > 0 && t('board.rest_idle', { n: groups.idle.length }), groups.stopped.length > 0 && t('board.rest_stopped', { n: groups.stopped.length }), offline > 0 && t('board.rest_offline', { n: offline })].filter(Boolean).join(' · ')}
              </button>
              {showRest && (
                <div className="bd-rest-list">
                  {[...groups.idle, ...groups.stopped].map((i) => (
                    <button key={i.id} type="button" className="bd-rest-row" onClick={() => openSession(i.id)}>
                      <SessionIcon inst={i} size={16} />
                      <span className="ellipsis">{i.chat?.title || i.name || baseName(i.workdir)}</span>
                      <span className="muted small">{t(boardState(i) === 'idle' ? 'board.idle' : 'board.stopped')}</span>
                    </button>
                  ))}
                </div>
              )}
            </div>
          )}
        </div>
        {!narrow && selected && (
          <aside className="bd-side" aria-label={t('board.detail')}>
            <div className="bd-side-head">
              <SessionIcon inst={selected} size={24} />
              <span className="grow">
                <b className="ellipsis">{selected.chat?.title || selected.name || baseName(selected.workdir)}</b>
                <span className="muted small ellipsis">{selected.workdir}</span>
              </span>
            </div>
            <Peek key={selected.id} i={selected} state={selState} onOpen={() => openSession(selected.id)} />
          </aside>
        )}
      </div>
    </div>
  );
}

/** 等你处理的会话个数（侧栏、顶栏的数字，标签页标题） */
export function useAttnCount() {
  const sessions = useSessions();
  return sessions ? sessions.items.filter((i) => boardState(i) === 'attn').length : 0;
}
