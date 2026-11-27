// 助理（docs/ASSISTANT.md，设计稿 docs/SINGLE_ENTRY_AGENT_DESIGN_2026-11-11.md 零期）：一个入口说话，助理自己判断是哪个项目、
// 新开会话还是接着原来的，把活派给 Claude Code / Codex；小事自己做。这一页 = 和助理的对话 + 它派出去在干的活 + 每次派到哪、为什么（去向条，能改）
// + 口头约定。助理不绑定某个工具：底下用 Claude Code 还是 Codex（引擎）、它自己动手时怎么管（权限模式）都在页头选，
// 两种引擎各是这台电脑上一个藏起来的对话模式会话（不在会话列表里），对话直接用 ChatView。
import { Activity, ArrowLeft, ArrowRightLeft, Bot, Check, ChevronRight, ExternalLink, Eye, FolderCheck, Hand, Hourglass, ListChecks, Loader2, MessageCircleQuestion, RotateCcw, Search, Send, Settings2, ShieldCheck, Sparkles, Square, X, Zap } from 'lucide-react';
import { useCallback, useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';

import { api, rememberTerminals, selectedDevice, terminalDevice } from '../shared/api.js';
import { LaunchIcon } from '../shared/brands.jsx';
import { DeviceSwitcher, useDevices } from '../shared/devices.jsx';
import { errorText } from '../shared/i18n.js';
import { isTouch } from '../shared/KeyBar.jsx';
import { Link, useRouter } from '../shared/router.jsx';
import Select from '../shared/Select.jsx';
import { effortsFor } from '../shared/chat.js';
import { AGENTS, baseName, sessionLaunch, sessionTitle, useSessions } from '../shared/sessions.jsx';
import { formatTime, relativeTime } from '../shared/time.js';
import { Button, CopyButton, Empty, Note, Skeleton, Switch, useToast } from '../shared/ui.jsx';
import ErrorBoundary from '../shared/ErrorBoundary.jsx';
import ChatView from './ChatView.jsx';
import { SetupAll } from './AgentUpdates.jsx';
import MemoryTab from './AssistantMemory.jsx';
import { AssistantOpen, useOpenSession } from '../shared/assistantOpen.js';
import AttnPeek from '../shared/AttnPeek.jsx';

// 引擎：大脑底下用的工具（界面、工具、规矩都一样，只是接法不同）
// 能当大脑的引擎
const ENGINES = ['claude', 'codex'];
const ENGINE_NAMES = { claude: 'Claude Code', codex: 'Codex', agy: 'Antigravity' };
// 能派活的工具（assistant.rs 的 TOOLS；/assistant 的 tools 到了以后照它更新，见 syncTools）
let DISPATCH_TOOLS = ['claude', 'codex', 'agy'];
// 权限模式下一轮才生效的工具
const NEXT_TURN = new Set(['codex']);
// 各工具的账号接口
const ACCOUNTS = { claude: ['/claude-profiles', 'cprof.builtin'], codex: ['/codex-profiles', 'xprof.builtin'], agy: ['/agy-profiles', 'aprof.builtin'] };
// 权限模式：助理自己动手（跑命令、改文件）时怎么办
const MODES = ['read_only', 'ask', 'auto'];
const MODE_ICON = { read_only: Eye, ask: Hand, auto: Zap };

const POLL_MS = 4000;
// 最近一次派活（这么久以内、没改过的）在对话上方单独显示一条，方便马上改
const FRESH_MS = 15 * 60 * 1000;

function useAssistant() {
  const [state, setState] = useState(null);
  const [error, setError] = useState(null);
  const load = useCallback(
    () =>
      api.get('/assistant').then(
        (d) => {
          const dev = selectedDevice();
          // 助理和它派的会话都在这台电脑上：记下来，对话接口才会发到这台
          if (dev) rememberTerminals(dev, [d.brain, ...d.tasks.map((x) => x.session), ...d.dispatches.map((x) => x.session)].filter(Boolean).map((id) => ({ id })));
          syncTools(d.tools);
          setState(d);
          setError(null);
        },
        (e) => setError(e),
      ),
    [],
  );
  useEffect(() => {
    load();
    const id = setInterval(() => document.visibilityState === 'visible' && load(), POLL_MS);
    // 在这页上批了 / 答了：马上取一次，会话状态晚一点才变的再补一次
    let later;
    const now = () => {
      load();
      clearTimeout(later);
      later = setTimeout(load, 800);
    };
    window.addEventListener('ll:attn-answered', now);
    return () => {
      clearInterval(id);
      clearTimeout(later);
      window.removeEventListener('ll:attn-answered', now);
    };
  }, [load]);
  return { state, error, load };
}

/**
 * 手机上软键盘弹着：iOS Safari / 新版 Android Chrome 只缩小可见区域，底部的输入框会被键盘盖住。
 * 键盘弹着时整页贴合可见区域（盖住顶栏和底部标签，收起后复原）。返回 style（没弹键盘时 undefined）。
 */
function useKeyboardFit(enabled) {
  const [fit, setFit] = useState(null);
  useEffect(() => {
    const vv = window.visualViewport;
    if (!enabled || !vv) return undefined;
    let full = 0;
    let w = 0;
    const on = () => {
      if (window.innerWidth !== w) {
        w = window.innerWidth;
        full = 0;
      }
      full = Math.max(full, vv.height, document.documentElement.clientHeight);
      setFit(vv.height < full - 120 ? { top: vv.offsetTop, height: vv.height } : null);
    };
    on();
    vv.addEventListener('resize', on);
    vv.addEventListener('scroll', on);
    return () => {
      vv.removeEventListener('resize', on);
      vv.removeEventListener('scroll', on);
    };
  }, [enabled]);
  return fit ? { position: 'fixed', left: 0, right: 0, top: fit.top, height: fit.height, zIndex: 45, background: 'var(--bg)' } : undefined;
}

// 右栏（活动）在宽屏上开着还是收起：记在这台设备上
const SIDE_KEY = 'll.asst.side';
const readSide = () => {
  try {
    return localStorage.getItem(SIDE_KEY) !== '0';
  } catch {
    return true;
  }
};
const wide = () => typeof window !== 'undefined' && window.matchMedia?.('(min-width: 1200px)').matches;

/**
 * 助理页：左边是和助理的对话（主角，一直能说话），右边是“活动”——它派出去的会话和后台跑腿（像 dots 的 Activity），
 * 分“等你 / 在干 / 做完了”。点哪件就在右边直接打开那个会话（能看、能回、能停），不用离开助理；约定、设置在右边另两页。
 * 宽屏右栏可以收起（记住）；窄屏右栏是盖上来的一层，页头的“活动”按钮打开，有等你处理的会标数字。
 */
export default function Assistant() {
  const { t } = useTranslation();
  const { state, error, load } = useAssistant();
  const [isWide, setWide] = useState(wide);
  const [sideOpen, setSideOpen] = useState(() => (wide() ? readSide() : false));
  const [tab, setTab] = useState('activity');
  // 在右边打开着的会话
  const [viewing, setViewing] = useState(null);
  const [touch] = useState(isTouch);
  const devices = useDevices();
  const fit = useKeyboardFit(touch);
  useEffect(() => {
    const mq = window.matchMedia?.('(min-width: 1200px)');
    if (!mq) return undefined;
    const on = () => {
      setWide(mq.matches);
      setSideOpen(mq.matches ? readSide() : false);
    };
    mq.addEventListener('change', on);
    return () => mq.removeEventListener('change', on);
  }, []);
  const setSide = useCallback(
    (v) => {
      setSideOpen(v);
      if (isWide) {
        try {
          localStorage.setItem(SIDE_KEY, v ? '1' : '0');
        } catch {
          // 存不了就只管这一次
        }
      }
    },
    [isWide],
  );
  const open = useCallback(
    (id) => {
      setViewing(id);
      setTab('activity');
      setSide(true);
    },
    [setSide],
  );
  if (!state) {
    return (
      <div className="asst asst-pad">
        {error ? <Note kind="error">{errorText(t, error)}</Note> : <Skeleton height={200} />}
      </div>
    );
  }
  if (!state.brain) return <Enable state={state} onDone={load} />;
  const waiting = state.tasks.filter((x) => x.state === 'waiting_for_user').length;
  const working = state.tasks.filter((x) => x.state === 'working').length;
  const engine = state.settings?.engine ?? 'claude';
  return (
    <AssistantOpen.Provider value={open}>
      <div className="asst" style={fit} data-side={sideOpen ? '' : undefined}>
        <div className="asst-main">
          <div className="asst-head">
            <span className="asst-title">
              <Sparkles size={16} aria-hidden="true" />
              <strong>{t('asst.title')}</strong>
            </span>
            <EnginePick state={state} onChange={load} />
            <ModePick state={state} onChange={load} />
            <span className="grow" />
            {/* 每台电脑一个助理；多台时这里换（手机上在顶栏） */}
            {(devices.multi || devices.localOnly) && (
              <span className="asst-dev">
                <DeviceSwitcher perDevice />
              </span>
            )}
            <RestartButton onDone={load} label />
            <button
              type="button"
              className="btn btn-plain btn-sm asst-side-btn"
              onClick={() => setSide(!sideOpen)}
              aria-pressed={sideOpen}
              aria-controls="asst-side"
              title={t(sideOpen ? 'asst.side_hide' : 'asst.side_show')}
            >
              {working > 0 ? <Loader2 size={15} className="spin" aria-hidden="true" /> : <Activity size={15} aria-hidden="true" />}
              <span className="asst-side-label">{t('asst.activity')}</span>
              {waiting > 0 ? <span className="rail-count asst-count">{waiting}</span> : working > 0 && <span className="asst-count-soft">{working}</span>}
            </button>
          </div>
          <FreshRoutes list={state.dispatches} onChange={load} />
          {/* 派出去的会话在等你批 / 答：对话上方就有，不用打开右栏、不用跳去会话。右栏正开着活动列表、或正看着那个会话时不重复 */}
          <WaitingStrip tasks={state.tasks} skip={sideOpen ? (viewing ?? (tab === 'activity' ? '*' : null)) : null} onOpen={open} onShowAll={() => {
            setViewing(null);
            setTab('activity');
            setSide(true);
          }} />
          <div className="asst-chat">
            <ErrorBoundary>
              {/* 换引擎就是换了一段对话：按大脑重新挂 */}
              <ChatView key={state.brain} id={state.brain} launch={engine} active touch={touch} label={t('asst.title')} />
            </ErrorBoundary>
          </div>
        </div>
        <aside id="asst-side" className="asst-side" data-open={sideOpen ? '' : undefined} data-viewing={viewing ? '' : undefined} aria-label={t('asst.activity')}>
          {viewing ? (
            <SessionPane id={viewing} tasks={state.tasks} dispatches={state.dispatches} touch={touch} onBack={() => setViewing(null)} onClose={() => setSide(false)} onChange={load} />
          ) : (
            <>
              <div className="asst-side-head">
                <div className="asst-tabs" role="tablist">
                  {['activity', 'memory', 'agreements', 'settings'].map((k) => (
                    <button key={k} type="button" role="tab" aria-selected={tab === k} className="asst-tab" onClick={() => setTab(k)}>
                      {t(`asst.tab.${k}`)}
                      {k === 'activity' && waiting > 0 && <span className="rail-count asst-count">{waiting}</span>}
                      {k === 'agreements' && state.agreements.length > 0 && <span className="asst-count-soft">{state.agreements.length}</span>}
                    </button>
                  ))}
                </div>
                <button type="button" className="btn btn-plain btn-icon asst-side-close" onClick={() => setSide(false)} aria-label={t('asst.side_hide')} title={t('asst.side_hide')}>
                  <X size={18} aria-hidden="true" />
                </button>
              </div>
              <div className="asst-side-body">
                {tab === 'activity' && <ActivityTab state={state} onOpen={setViewing} onChange={load} />}
                {tab === 'memory' && <MemoryTab />}
                {tab === 'agreements' && <AgreementsTab state={state} onChange={load} />}
                {tab === 'settings' && <SettingsTab state={state} onChange={load} />}
              </div>
            </>
          )}
        </aside>
      </div>
    </AssistantOpen.Provider>
  );
}

/** 强制重启：结束助理的进程，按现在的默认账号、Key 重新起（换了 Key 没生效、卡住时用）；卡在账号问题上的接着跑 */
function RestartButton({ onDone, label = false }) {
  const { t } = useTranslation();
  const toast = useToast();
  const [busy, setBusy] = useState(false);
  const go = () => {
    setBusy(true);
    api
      .post('/assistant/restart')
      .then(
        (r) => {
          toast(t(r.continued ? 'asst.restarted_go_on' : 'asst.restarted', { name: r.name || t('asst.account_builtin') }), 'success');
          onDone();
        },
        (e) => toast(errorText(t, e), 'error'),
      )
      .finally(() => setBusy(false));
  };
  return (
    <button
      type="button"
      className={`btn btn-plain btn-sm${label ? ' asst-restart' : ' btn-icon'}`}
      onClick={go}
      disabled={busy}
      aria-label={t('asst.restart')}
      title={t('asst.restart_hint')}
    >
      <RotateCcw size={15} aria-hidden="true" className={busy ? 'spin' : undefined} />
      {label && <span className="asst-restart-label">{t('asst.restart')}</span>}
    </button>
  );
}

/** 换设置（引擎 / 权限模式）。成功了说一句换了什么 */
function useSaveSettings(onChange) {
  const { t } = useTranslation();
  const toast = useToast();
  const [busy, setBusy] = useState(false);
  const save = (body, done) => {
    setBusy(true);
    return api
      .put('/assistant/settings', body)
      .then(
        (r) => {
          toast(done(r), 'success');
          onChange();
        },
        (e) => toast(e?.code === 'ASSISTANT_UNAVAILABLE' ? t('asst.unavailable', { name: ENGINE_NAMES[body.engine] ?? '' }) : errorText(t, e), 'error'),
      )
      .finally(() => setBusy(false));
  };
  return { busy, save };
}

/** 页头：底下用哪个工具。换了是另一段对话，换回来接着原来的 */
function EnginePick({ state, onChange }) {
  const { t } = useTranslation();
  const { busy, save } = useSaveSettings(onChange);
  const cur = state.settings?.engine ?? 'claude';
  const engines = state.engines ?? ENGINES.map((id) => ({ id, installed: true }));
  const options = engines.map((e) => [
    e.id,
    <span key={e.id} className="asst-opt">
      <LaunchIcon launch={e.id} size={15} />
      {ENGINE_NAMES[e.id] ?? e.id}
    </span>,
    { disabled: !e.installed, hint: e.installed ? (e.has_brain || e.id === cur ? undefined : t('asst.engine_new')) : t('asst.engine_missing') },
  ]);
  return (
    <Select
      className="asst-pick"
      value={cur}
      disabled={busy}
      options={options}
      onChange={(e) => {
        const engine = e.target.value;
        if (engine !== cur) save({ engine }, () => t('asst.engine_switched', { name: ENGINE_NAMES[engine] }));
      }}
      aria-label={t('asst.engine')}
      title={t('asst.engine_hint')}
    />
  );
}

/** 页头：权限模式（助理自己动手时怎么办；派出去的会话按它们自己的权限） */
function ModePick({ state, onChange }) {
  const { t } = useTranslation();
  const { busy, save } = useSaveSettings(onChange);
  const cur = state.settings?.mode ?? 'ask';
  const options = MODES.map((m) => {
    const Icon = MODE_ICON[m];
    return [
      m,
      <span key={m} className="asst-opt">
        <Icon size={14} aria-hidden="true" />
        {t(`asst.mode.${m}`)}
      </span>,
      { hint: t(`asst.mode.${m}_hint`) },
    ];
  });
  return (
    <Select
      className="asst-pick"
      data-mode={cur}
      value={cur}
      disabled={busy}
      options={options}
      onChange={(e) => {
        const mode = e.target.value;
        if (mode !== cur) save({ mode }, () => t('asst.mode_switched', { name: t(`asst.mode.${mode}`) }));
      }}
      aria-label={t('asst.mode_label')}
      title={t('asst.mode_label')}
    />
  );
}

/** 还没开：说清楚它是什么、怎么用；选好底下用哪个工具、权限模式，点一下建好 */
function Enable({ state, onDone }) {
  const { t } = useTranslation();
  const devices = useDevices();
  const toast = useToast();
  const engines = state.engines ?? ENGINES.map((id) => ({ id, installed: true }));
  const firstInstalled = engines.find((e) => e.installed)?.id;
  const saved = state.settings?.engine;
  const [engine, setEngine] = useState(engines.find((e) => e.id === saved && e.installed) ? saved : (firstInstalled ?? 'claude'));
  const [mode, setMode] = useState(state.settings?.mode ?? 'ask');
  const [busy, setBusy] = useState(false);
  const go = () => {
    setBusy(true);
    api
      .post('/assistant/enable', { engine, mode })
      .then(onDone, (e) => toast(e?.code === 'ASSISTANT_UNAVAILABLE' ? t('asst.unavailable', { name: ENGINE_NAMES[engine] }) : errorText(t, e), 'error'))
      .finally(() => setBusy(false));
  };
  return (
    <div className="asst asst-pad">
      {(devices.multi || devices.localOnly) && (
        <span className="asst-dev">
          <DeviceSwitcher perDevice />
        </span>
      )}
      <Empty icon={Sparkles} title={t('asst.intro_title')}>
        <ul className="asst-intro">
          <li>{t('asst.intro_1')}</li>
          <li>{t('asst.intro_2')}</li>
          <li>{t('asst.intro_3')}</li>
          <li>{t('asst.intro_4')}</li>
          <li>{t('asst.intro_5')}</li>
        </ul>
        <div className="asst-setup">
          <h3 className="asst-h">{t('asst.engine')}</h3>
          <p className="muted asst-small">{t('asst.engine_hint')}</p>
          <div className="asst-cards" role="radiogroup" aria-label={t('asst.engine')}>
            {engines.map((e) => (
              <button key={e.id} type="button" role="radio" aria-checked={engine === e.id} disabled={!e.installed} className="launch model-option asst-card" onClick={() => setEngine(e.id)}>
                <span className="model-option-head">
                  <LaunchIcon launch={e.id} size={18} />
                  <b className="grow">{ENGINE_NAMES[e.id] ?? e.id}</b>
                  {engine === e.id && <Check size={16} aria-hidden="true" />}
                </span>
                <span className="small muted">{e.installed ? t(`asst.engine_desc.${e.id}`) : t('asst.engine_missing')}</span>
              </button>
            ))}
          </div>
          <SetupAll items={engines.map((e) => ({ tool: e.id, installed: e.installed }))} onChanged={onDone} />
          <h3 className="asst-h">{t('asst.mode_label')}</h3>
          <p className="muted asst-small">{t('asst.mode_intro')}</p>
          <div className="asst-cards asst-cards-3" role="radiogroup" aria-label={t('asst.mode_label')}>
            {MODES.map((m) => {
              const Icon = MODE_ICON[m];
              return (
                <button key={m} type="button" role="radio" aria-checked={mode === m} className="launch model-option asst-card" onClick={() => setMode(m)}>
                  <span className="model-option-head">
                    <Icon size={16} aria-hidden="true" />
                    <b className="grow">{t(`asst.mode.${m}`)}</b>
                    {m === 'ask' && <span className="badge">{t('asst.recommended')}</span>}
                  </span>
                  <span className="small muted">{t(`asst.mode.${m}_hint`)}</span>
                </button>
              );
            })}
          </div>
        </div>
        <Button onClick={go} busy={busy} icon={Sparkles} disabled={!engines.find((e) => e.id === engine)?.installed}>
          {t('asst.enable')}
        </Button>
        <p className="muted asst-note">{t('asst.enable_note')}</p>
      </Empty>
    </div>
  );
}

const STATE_ICON = { working: Loader2, waiting_for_user: MessageCircleQuestion, turn_finished: Check };
const GROUPS = [
  ['waiting', (x) => x.state === 'waiting_for_user'],
  ['working', (x) => x.state === 'working'],
  ['done', (x) => x.state !== 'waiting_for_user' && x.state !== 'working'],
];

/** 活动：派出去的会话和跑腿，按“等你 / 在干 / 做完了”分组；下面是每次派到哪（能改）。最上面是“派出去的会话的权限”开关 */
function ActivityTab({ state, onOpen, onChange }) {
  const { t } = useTranslation();
  const groups = GROUPS.map(([k, f]) => [k, state.tasks.filter(f)]).filter(([, l]) => l.length > 0);
  const { fixed, of } = state.stats;
  return (
    <>
      <Waits waits={state.waits ?? []} onOpen={onOpen} onChange={onChange} />
      {groups.length === 0 ? (state.waits ?? []).length > 0 ? null : (
        <p className="muted asst-small asst-empty">{t('asst.activity_empty')}</p>
      ) : (
        groups.map(([k, list]) => (
          <section key={k}>
            <h3 className="asst-h asst-h-sm">
              {t(`asst.group.${k}`)} <span className="muted">{list.length}</span>
              {/* 做完的会越攒越多：一下清掉（只是不再列在这里，会话还在） */}
              {k === 'done' && (
                <button type="button" className="btn btn-plain btn-sm asst-h-act" onClick={() => Promise.all(list.map((x) => api.del(`/assistant/watch/${x.session}`).catch(() => {}))).then(onChange)}>
                  {t('asst.clear_done')}
                </button>
              )}
            </h3>
            <div className="asst-tasks">
              {list.map((x) => (
                <TaskCard key={x.session} x={x} onOpen={onOpen} onChange={onChange} />
              ))}
            </div>
          </section>
        ))
      )}
      {/* 等你的排在最上面（手机上一打开就看到），权限上限跟在活后面 */}
      <CeilingPick state={state} onChange={onChange} />
      <section>
        <h3 className="asst-h asst-h-sm">
          <ArrowRightLeft size={14} aria-hidden="true" />
          {t('asst.routes')}
        </h3>
        {of > 0 && <p className="muted asst-small">{t('asst.stats', { of, fixed })}</p>}
        {state.dispatches.length === 0 ? (
          <p className="muted asst-small">{t('asst.routes_empty')}</p>
        ) : (
          <div className="asst-routes">
            {state.dispatches.slice(0, 50).map((d) => (
              <Route key={d.id} d={d} onChange={onChange} />
            ))}
          </div>
        )}
      </section>
    </>
  );
}

/** 在等的事（docs/ASSISTANT.md「等着的事」）：等谁、等什么、好了做什么；对上了助理会被叫醒。不用等了点 × */
function Waits({ waits, onOpen, onChange }) {
  const { t } = useTranslation();
  const toast = useToast();
  if (waits.length === 0) return null;
  const drop = (id) => api.post('/assistant/waits/remove', { id }).then(onChange, (e) => toast(errorText(t, e), 'error'));
  return (
    <section>
      <h3 className="asst-h asst-h-sm">
        {t('asst.group.waits')} <span className="muted">{waits.length}</span>
      </h3>
      <ul className="asst-agree">
        {waits.map((w) => (
          <li key={w.id}>
            <Hourglass size={14} aria-hidden="true" className="asst-agree-ico" />
            <span>
              {w.who?.name && <strong>{w.who.name} · </strong>}
              {w.git?.ref && <code className="asst-wait-ref">{w.git.ref}</code>}
              {w.git?.ref && ' · '}
              {w.what}
              <span className="muted asst-wait-sub">
                {w.then ? t(w.go ? 'asst.wait.then_go' : 'asst.wait.then_ask', { then: w.then }) : t('asst.wait.then_tell')}
                {w.overdue_told && <> · {t('asst.wait.overdue')}</>}
                {w.signals >= 5 && <> · {t('asst.wait.exhausted')}</>}
                {w.session && (
                  <>
                    {' · '}
                    <button type="button" className="link-btn asst-link" onClick={() => onOpen(w.session)}>
                      {t('asst.wait.session')}
                    </button>
                  </>
                )}
              </span>
            </span>
            <button type="button" className="btn btn-plain btn-icon btn-sm" onClick={() => drop(w.id)} aria-label={t('asst.wait.remove')} title={t('asst.wait.remove')}>
              <X size={14} aria-hidden="true" />
            </button>
          </li>
        ))}
      </ul>
    </section>
  );
}

/** 一件活：状态、标题（跑腿带标签）、项目、它正在做的；点了在右边打开；在干的能停，做完的能不再盯着 */
function TaskCard({ x, onOpen, onChange }) {
  const { t } = useTranslation();
  // 看过的“做完了”会变成空着：在这里照样叫做完了
  const state = x.state === 'idle' ? 'turn_finished' : x.state;
  const Icon = x.attn?.kind === 'permission' ? ShieldCheck : x.errand && state !== 'working' ? Search : (STATE_ICON[state] ?? Bot);
  const stop = (e) => {
    e.stopPropagation();
    api.post(`/instances/${x.session}/chat/interrupt`).then(onChange, () => {});
  };
  const drop = (e) => {
    e.stopPropagation();
    api.del(`/assistant/watch/${x.session}`).then(onChange, () => {});
  };
  const attn = x.state === 'waiting_for_user' && x.attn ? x.attn : null;
  return (
    <div className="asst-task" data-state={x.state} data-attn={attn ? '' : undefined}>
      <div className="asst-task-row">
      <button type="button" className="asst-task-main" onClick={() => onOpen(x.session)}>
        <Icon size={15} aria-hidden="true" className={x.state === 'working' ? 'spin' : undefined} />
        <span className="asst-task-body">
          <span className="asst-task-title">
            {x.errand && <span className="asst-chip-tag">{t('asst.errand')}</span>}
            <span className="ellipsis">{x.title}</span>
          </span>
          <span className="asst-task-sub ellipsis">
            {t(`asst.state.${state}`, { defaultValue: state })} · {baseName(x.project) || x.project}
            {x.doing && x.state === 'working' ? ` · ${x.doing}` : ''}
            {x.state !== 'working' && x.last_active ? ` · ${relativeTime(t, x.last_active)}` : ''}
          </span>
          {/* 回来时一眼看到结果，不用一个个点开 */}
          {x.state !== 'working' && x.last_summary && <span className="asst-task-sum">{x.last_summary}</span>}
        </span>
      </button>
      <LevelPick x={x} onChange={onChange} />
      {x.state === 'working' && (
        <button type="button" className="btn btn-plain btn-icon btn-sm" onClick={stop} aria-label={t('asst.stop')} title={t('asst.stop')}>
          <Square size={12} aria-hidden="true" />
        </button>
      )}
      {x.state !== 'working' && x.state !== 'waiting_for_user' && (
        <button type="button" className="btn btn-plain btn-icon btn-sm" onClick={drop} aria-label={t('asst.unwatch')} title={t('asst.unwatch')}>
          <X size={14} aria-hidden="true" />
        </button>
      )}
      </div>
      {/* 在等你的那件事就地处理：是什么命令 / 改哪些文件、允许 / 拒绝 / 回答，看不明白再点开会话 */}
      {attn && <AttnPeek id={x.session} attn={attn} agent={ENGINE_NAMES[x.engine] ?? x.engine} launch={x.engine} compact />}
    </div>
  );
}

/**
 * 对话上方的“等你处理”：派出去的会话、跑腿在等你批准 / 回答时，第一件直接摆出来就地处理，其余的点“还有 N 件”去活动面板。
 * 手机上右栏是收着的，这条不放这里用户就看不到。`skip`：右栏正在显示的（那个会话 id；'*' = 活动列表开着，全都不用重复）
 */
function WaitingStrip({ tasks, skip, onOpen, onShowAll }) {
  const { t } = useTranslation();
  const [hidden, setHidden] = useState(() => new Set());
  if (skip === '*') return null;
  const list = tasks
    .filter((x) => x.state === 'waiting_for_user' && x.attn && x.session !== skip)
    .sort((a, b) => (a.since ?? 0) - (b.since ?? 0));
  const key = (x) => `${x.session}|${x.attn.pending_id ?? x.attn.kind}`;
  const shown = list.filter((x) => !hidden.has(key(x)));
  if (shown.length === 0) return null;
  const x = shown[0];
  const more = list.length - 1;
  return (
    <section className="asst-needs" aria-label={t('asst.attn.title')} aria-live="polite">
      <div className="asst-needs-head">
        <ShieldCheck size={15} aria-hidden="true" className="asst-needs-ico" />
        <span className="asst-needs-title">
          {x.errand && <span className="asst-chip-tag">{t('asst.errand')}</span>}
          <button type="button" className="link-btn asst-link ellipsis" onClick={() => onOpen(x.session)}>
            {x.title}
          </button>
          <span className="muted ellipsis"> · {baseName(x.project) || x.project}</span>
        </span>
        {more > 0 && (
          <button type="button" className="btn btn-plain btn-sm asst-needs-more" onClick={onShowAll}>
            {t('asst.attn.more', { n: more })}
          </button>
        )}
        <button type="button" className="btn btn-plain btn-icon btn-sm" onClick={() => setHidden(new Set([...hidden, key(x)]))} aria-label={t('asst.attn.later')} title={t('asst.attn.later')}>
          <X size={14} aria-hidden="true" />
        </button>
      </div>
      {/* 看完整的上下文、改动：点标题在右栏打开那个会话 */}
      <AttnPeek key={key(x)} id={x.session} attn={x.attn} agent={ENGINE_NAMES[x.engine] ?? x.engine} launch={x.engine} compact />
    </section>
  );
}

/** 在右边打开的会话：能看、能直接回它、能停；← 回到活动，↗ 去会话页 */
function SessionPane({ id, tasks, dispatches, touch, onBack, onClose, onChange }) {
  const { t } = useTranslation();
  const { navigate } = useRouter();
  const task = tasks.find((x) => x.session === id);
  const d = dispatches.find((x) => x.session === id);
  const title = task?.title ?? d?.title ?? id;
  return (
    <div className="asst-pane">
      <div className="asst-side-head asst-pane-head">
        <button type="button" className="btn btn-plain btn-icon" onClick={onBack} aria-label={t('asst.back_activity')} title={t('asst.back_activity')}>
          <ArrowLeft size={17} aria-hidden="true" />
        </button>
        <span className="asst-pane-title">
          {task?.errand && <span className="asst-chip-tag">{t('asst.errand')}</span>}
          <b className="ellipsis">{title}</b>
        </span>
        {task && <LevelPick x={task} onChange={onChange} />}
        <button type="button" className="btn btn-plain btn-icon" onClick={() => navigate(`/s/${id}`)} aria-label={t('asst.open_full')} title={t('asst.open_full')}>
          <ExternalLink size={16} aria-hidden="true" />
        </button>
        <button type="button" className="btn btn-plain btn-icon asst-side-close" onClick={onClose} aria-label={t('asst.side_hide')}>
          <X size={18} aria-hidden="true" />
        </button>
      </div>
      {d?.reason && <p className="asst-pane-why">{t('asst.route.why', { reason: d.reason })}</p>}
      <div className="asst-pane-chat">
        <ErrorBoundary>
          <ChatView key={id} id={id} launch={task?.engine ?? d?.engine} active touch={touch} />
        </ErrorBoundary>
      </div>
    </div>
  );
}

/** 派出去的会话的权限分四档（docs/ASSISTANT.md「派出去的会话的权限」），两种引擎同一个意思 */
const LEVELS = ['read_only', 'ask', 'project', 'full'];
const LEVEL_ICON = { read_only: Eye, ask: Hand, project: FolderCheck, full: Zap };
const levelOptions = (t) =>
  LEVELS.map((m) => {
    const Icon = LEVEL_ICON[m];
    return [
      m,
      <span key={m} className="asst-opt">
        <Icon size={14} aria-hidden="true" />
        {t(`asst.level.${m}`)}
      </span>,
      { hint: t(`asst.level.${m}_hint`) },
    ];
  });

/** 派出去的会话最多到哪一档：助理按每件活自己挑（查原因只看、改代码项目内放手），超过这里的要你点头；跟助理自己动手的权限分开 */
function CeilingPick({ state, onChange }) {
  const { t } = useTranslation();
  const { busy, save } = useSaveSettings(onChange);
  const cur = state.ceiling ?? 'project';
  return (
    <div className="asst-inherit">
      <div className="asst-switch">
        <span>{t('asst.ceiling')}</span>
        <Select
          className="asst-pick"
          value={cur}
          disabled={busy}
          options={levelOptions(t)}
          onChange={(e) => {
            const v = e.target.value;
            if (v !== cur) save({ ceiling: v }, () => t('asst.ceiling_switched', { name: t(`asst.level.${v}`) }));
          }}
          aria-label={t('asst.ceiling')}
          title={t('asst.ceiling')}
        />
      </div>
      <p className="muted asst-small">{t('asst.ceiling_hint')}</p>
    </div>
  );
}

/** 每种工具能切的全部权限模式（assistant.rs 的 TOOLS）和各自算哪一档；这里是还没拿到 /assistant 时的底 */
const CLAUDE_MODES = [['plan', 'read_only'], ['default', 'ask'], ['acceptEdits', 'ask'], ['auto', 'project'], ['bypassPermissions', 'full']];
const CLAUDE_LEVELS = { read_only: 'plan', ask: 'acceptEdits', project: 'auto', full: 'bypassPermissions' };
const ENGINE_MODES = {
  claude: CLAUDE_MODES,
  codex: [['readOnly', 'read_only'], ['askCommands', 'ask'], ['workspaceWrite', 'project'], ['fullGuarded', 'full'], ['fullAccess', 'full']],
  agy: CLAUDE_MODES,
};
// 一档在这种工具上默认用的模式（assistant.rs 的 native_mode）
const LEVEL_MODE = {
  claude: CLAUDE_LEVELS,
  codex: { read_only: 'readOnly', ask: 'askCommands', project: 'workspaceWrite', full: 'fullGuarded' },
  agy: CLAUDE_LEVELS,
};
/** 后端的能力表到了：工具、名字、模式照它的 */
function syncTools(tools) {
  if (!Array.isArray(tools) || tools.length === 0) return;
  DISPATCH_TOOLS = tools.map((x) => x.id);
  for (const x of tools) {
    if (x.name) ENGINE_NAMES[x.id] = x.name;
    if (Array.isArray(x.modes) && x.modes.length) ENGINE_MODES[x.id] = x.modes;
    if (x.by_level) LEVEL_MODE[x.id] = x.by_level;
    if (x.mode_next_turn) NEXT_TURN.add(x.id);
    else NEXT_TURN.delete(x.id);
  }
}
const modeName = (t, engine, m) => (engine === 'codex' && m === 'workspaceWrite' ? t('asst.mode_workspaceWrite') : t(`chat.mode_${m}`));
/** 这种引擎的全部权限模式，图标按它算的那一档 */
const engineModeOptions = (t, engine) =>
  (ENGINE_MODES[engine] ?? ENGINE_MODES.claude).map(([m, level]) => {
    const Icon = LEVEL_ICON[level];
    return [
      m,
      <span key={m} className="asst-opt">
        <Icon size={14} aria-hidden="true" />
        {modeName(t, engine, m)}
      </span>,
      { hint: `${t(`asst.level.${level}`)} · ${t(engine === 'agy' && m === 'auto' ? 'asst.mode_hint_agy_auto' : `chat.mode_hint_${m}`)}` },
    ];
  });
const levelOfMode = (engine, m) => (ENGINE_MODES[engine] ?? []).find(([x]) => x === m)?.[1];

/** 一个派出去的会话现在是什么权限，点开能换成这种引擎的任何一种（调高也行：是你自己点的）。Claude Code 马上生效，Codex 下一轮生效 */
function LevelPick({ x, onChange }) {
  const { t } = useTranslation();
  const toast = useToast();
  const [busy, setBusy] = useState(false);
  const engine = x.engine;
  if (!ENGINE_MODES[engine] || x.errand) return null;
  // 现在实际的模式（对话页设置栏里换过的也算）；还没起来时按助理给的那一档
  const cur = levelOfMode(engine, x.mode) ? x.mode : x.level ? LEVEL_MODE[engine][x.level] : null;
  const set = (mode) => {
    setBusy(true);
    api
      .post(`/assistant/sessions/${x.session}/level`, { mode })
      .then(
        (r) => {
          toast(t(NEXT_TURN.has(engine) ? 'asst.level_switched_next' : 'asst.level_switched', { name: modeName(t, engine, r?.mode ?? mode) }), 'success');
          onChange?.();
        },
        (e) => toast(errorText(t, e), 'error'),
      )
      .finally(() => setBusy(false));
  };
  return (
    <span className="asst-level" onClick={(e) => e.stopPropagation()}>
      <Select
        className="asst-pick asst-pick-sm"
        data-level={levelOfMode(engine, cur) ?? x.level}
        value={cur ?? ''}
        placeholder={t('asst.level_label')}
        disabled={busy}
        options={engineModeOptions(t, engine)}
        onChange={(e) => e.target.value !== cur && set(e.target.value)}
        aria-label={t('asst.level_label')}
        title={t('asst.level_label')}
      />
    </span>
  );
}

/**
 * 设置里：派出去的 Claude Code / Codex 会话默认用什么权限、什么模型。改了马上存，下一次派活就用；
 * 助理按用户的话指定了的（“这个只看”“用 opus”）照指定的
 */
function DispatchDefaults({ state, onChange }) {
  const { t } = useTranslation();
  const engines = (state.tools ?? state.engines ?? []).filter((e) => e.installed).map((e) => e.id);
  const list = engines.length ? engines : ENGINES;
  return (
    <section>
      <h3 className="asst-h asst-h-sm">
        <Send size={14} aria-hidden="true" />
        {t('asst.defaults')}
      </h3>
      <p className="muted asst-small">{t('asst.defaults_hint')}</p>
      <DispatchEngine state={state} installed={engines} onChange={onChange} />
      {list.map((e) => (
        <EngineDefaults key={e} engine={e} cur={state.settings?.[e] ?? {}} onChange={onChange} />
      ))}
    </section>
  );
}

/** 派活默认用哪个工具：自动（装了 Claude Code 用它，否则 Codex）或固定一个；助理按你的话指定了的照你说的 */
function DispatchEngine({ state, installed, onChange }) {
  const { t } = useTranslation();
  const { busy, save } = useSaveSettings(onChange);
  const cur = state.settings?.dispatch_engine ?? '';
  const opts = [
    ['', t('asst.dispatch_engine_auto'), { hint: t('asst.dispatch_engine_auto_hint') }],
    ...DISPATCH_TOOLS.filter((e) => installed.includes(e) || e === cur).map((e) => [
      e,
      <span key={e} className="asst-opt">
        <LaunchIcon launch={e} size={14} />
        {ENGINE_NAMES[e]}
      </span>,
      installed.includes(e) ? undefined : { hint: t('asst.dispatch_engine_missing') },
    ]),
  ];
  return (
    <div className="asst-switch">
      <span>{t('asst.dispatch_engine')}</span>
      <Select
        className="asst-pick"
        value={cur}
        disabled={busy}
        options={opts}
        onChange={(e) => {
          const v = e.target.value;
          if (v !== cur) save({ dispatch_engine: v }, () => t('asst.dispatch_engine_saved', { name: v ? ENGINE_NAMES[v] : t('asst.dispatch_engine_auto') }));
        }}
        aria-label={t('asst.dispatch_engine')}
        title={t('asst.dispatch_engine')}
      />
    </div>
  );
}

function EngineDefaults({ engine, cur, onChange }) {
  const { t } = useTranslation();
  const { busy, save } = useSaveSettings(onChange);
  const [models, setModels] = useState(null);
  const [accounts, setAccounts] = useState(null);
  const account = cur.account ?? '';
  useEffect(() => {
    let live = true;
    api.get((ACCOUNTS[engine] ?? ACCOUNTS.claude)[0]).then(
      (r) => live && setAccounts(r ?? { profiles: [] }),
      () => live && setAccounts({ profiles: [] }),
    );
    return () => {
      live = false;
    };
  }, [engine]);
  useEffect(() => {
    let live = true;
    setModels(null);
    // 模型列表按选的账号拉（第三方接口的模型不一样）
    api.get(`/draft/models?agent=${engine}&profile=${encodeURIComponent(account)}`).then(
      (r) => live && setModels(r?.options ?? []),
      () => live && setModels([]),
    );
    return () => {
      live = false;
    };
  }, [engine, account]);
  const name = ENGINE_NAMES[engine];
  const acctName = (x) => x.name || t((ACCOUNTS[engine] ?? ACCOUNTS.claude)[1]);
  const acctList = accounts?.profiles ?? [];
  const defAcct = acctList.find((x) => x.id === accounts?.default);
  const acctNone = t('asst.defaults_account_none', { name: defAcct ? acctName(defAcct) : '' });
  const acctOpts = [
    ['', acctNone, { hint: t('asst.defaults_account_none_hint') }],
    ...acctList.map((x) => [x.id, acctName(x), x.kind === 'api' ? { hint: t('asst.defaults_account_api') } : undefined]),
  ];
  const put = (key, v, label) => save({ defaults: { [engine]: { [key]: v } } }, () => t('asst.defaults_saved', { engine: name, name: label }));
  const modeOpts = [['', t('asst.defaults_mode_none', { name: t('asst.level.project') }), { hint: t('asst.defaults_mode_none_hint') }], ...engineModeOptions(t, engine)];
  const known = (models ?? []).filter((o) => o.value && o.value !== 'default');
  const modelOpts = [
    ['', t('asst.defaults_model_none'), { hint: t('asst.defaults_model_none_hint') }],
    ...known.map((o) => [o.value, o.label ?? o.value, o.description ? { hint: o.description } : undefined]),
    // 存的模型不在现在的列表里（换了服务商、列表没拉到）：照样显示，能换掉
    ...(cur.model && !known.some((o) => o.value === cur.model) ? [[cur.model, cur.model]] : []),
  ];
  // 强度按选的模型（没选：工具的默认模型）能用的列；存的那个不在列表里也照样显示，能换掉
  const effortModel = known.find((o) => o.value === cur.model) ?? (models ?? []).find((o) => o.value === 'default') ?? null;
  const effortList = effortsFor(engine, effortModel);
  const effortLabel = (x) => t(`chat.effort_${x}`, { defaultValue: x });
  const effortOpts = [
    ['', t('asst.defaults_effort_none'), { hint: t('asst.defaults_effort_none_hint') }],
    ...effortList.map((x) => [x, effortLabel(x)]),
    ...(cur.effort && !effortList.includes(cur.effort) ? [[cur.effort, effortLabel(cur.effort)]] : []),
  ];
  return (
    <div className="asst-defaults">
      <div className="asst-defaults-name">
        <LaunchIcon launch={engine} size={15} />
        <b>{name}</b>
      </div>
      {acctList.length > 1 || account ? (
        <div className="asst-switch">
          <span>{t('asst.defaults_account')}</span>
          <Select
            className="asst-pick"
            value={acctList.some((x) => x.id === account) ? account : ''}
            disabled={busy || accounts == null}
            options={acctOpts}
            onChange={(e) => {
              const v = e.target.value;
              if (v !== account) put('account', v, v ? acctName(acctList.find((x) => x.id === v) ?? { name: v }) : acctNone);
            }}
            aria-label={`${name} ${t('asst.defaults_account')}`}
          />
        </div>
      ) : null}
      <div className="asst-switch">
        <span>{t('asst.defaults_mode')}</span>
        <Select
          className="asst-pick"
          value={cur.mode ?? ''}
          disabled={busy}
          options={modeOpts}
          onChange={(e) => {
            const v = e.target.value;
            if (v !== (cur.mode ?? '')) put('mode', v, v ? modeName(t, engine, v) : t('asst.defaults_mode_none', { name: t('asst.level.project') }));
          }}
          aria-label={`${name} ${t('asst.defaults_mode')}`}
        />
      </div>
      <div className="asst-switch">
        <span>{t('asst.defaults_model')}</span>
        <Select
          className="asst-pick"
          value={cur.model ?? ''}
          disabled={busy || models == null}
          options={modelOpts}
          onChange={(e) => {
            const v = e.target.value;
            if (v !== (cur.model ?? '')) put('model', v, v ? (known.find((o) => o.value === v)?.label ?? v) : t('asst.defaults_model_none'));
          }}
          aria-label={`${name} ${t('asst.defaults_model')}`}
        />
      </div>
      {effortList.length > 0 || cur.effort ? (
        <div className="asst-switch">
          <span>{t('asst.defaults_effort')}</span>
          <Select
            className="asst-pick"
            value={cur.effort ?? ''}
            disabled={busy || models == null}
            options={effortOpts}
            onChange={(e) => {
              const v = e.target.value;
              if (v !== (cur.effort ?? '')) put('effort', v, v ? effortLabel(v) : t('asst.defaults_effort_none'));
            }}
            aria-label={`${name} ${t('asst.defaults_effort')}`}
          />
        </div>
      ) : null}
    </div>
  );
}

/** 刚派出去的那几次：对话上方各一条去向条，写去了哪、为什么，能马上改 */
function FreshRoutes({ list, onChange }) {
  const [now, setNow] = useState(Date.now);
  const [hidden, setHidden] = useState(() => new Set());
  useEffect(() => {
    const id = setInterval(() => setNow(Date.now()), 30000);
    return () => clearInterval(id);
  }, []);
  const fresh = list.filter((d) => !d.fixed && now - d.at < FRESH_MS && !hidden.has(d.id)).slice(0, 1);
  if (fresh.length === 0) return null;
  return (
    <div className="asst-fresh">
      {fresh.map((d) => (
        <Route key={d.id} d={d} onChange={onChange} onHide={() => setHidden(new Set([...hidden, d.id]))} compact />
      ))}
    </div>
  );
}

/** 一条去向：去了哪个会话（新开 / 接着）、哪个项目、为什么；“不对”可以改成新开或者改发给另一个会话 */
function Route({ d, onChange, onHide, compact = false }) {
  const { t } = useTranslation();
  const toast = useToast();
  const { navigate } = useRouter();
  const openSession = useOpenSession(navigate);
  const sessions = useSessions();
  const [fixing, setFixing] = useState(false);
  const [busy, setBusy] = useState(false);
  const [target, setTarget] = useState('');
  const dev = selectedDevice();
  const choices = useMemo(
    () =>
      (sessions.items ?? [])
        .filter((i) => i.id !== d.session && AGENTS.includes(sessionLaunch(i)) && (!sessions.multi || terminalDevice(i.id) === dev))
        .slice(0, 60)
        .map((i) => [i.id, `${sessionTitle(i)} · ${baseName(i.workdir)}`]),
    [sessions.items, sessions.multi, d.session, dev],
  );
  const fix = (mode) => {
    setBusy(true);
    api
      .post(`/assistant/dispatches/${d.id}/fix`, mode === 'into' ? { mode, session: target } : { mode })
      .then(
        () => {
          setFixing(false);
          toast(t('asst.fixed'), 'success');
          onChange();
        },
        (e) => toast(errorText(t, e), 'error'),
      )
      .finally(() => setBusy(false));
  };
  const queued = !d.session && d.queued_after;
  const where = d.kind === 'start' ? (queued ? 'asst.route.queued' : 'asst.route.new') : 'asst.route.into';
  return (
    <div className="asst-route" data-compact={compact ? '' : undefined} data-fixed={d.fixed ? '' : undefined}>
      <div className="asst-route-line">
        <ArrowRightLeft size={14} aria-hidden="true" className="asst-route-ico" />
        <span className="asst-route-text">
          {t(where)}{' '}
          {d.session ? (
            <button type="button" className="link-btn asst-link" onClick={() => openSession(d.session)}>
              {d.title}
            </button>
          ) : (
            <strong>{d.title}</strong>
          )}
          <span className="muted"> · {baseName(d.project) || d.project}</span>
          {!compact && <span className="muted"> · {formatTime(d.at)}</span>}
        </span>
        {!d.fixed && !fixing && (
          <button type="button" className="btn btn-plain btn-sm" onClick={() => setFixing(true)}>
            {t('asst.route.wrong')}
          </button>
        )}
        {onHide && !fixing && (
          <button type="button" className="btn btn-plain btn-icon btn-sm" onClick={onHide} aria-label={t('asst.route.ok')} title={t('asst.route.ok')}>
            <Check size={15} aria-hidden="true" />
          </button>
        )}
      </div>
      {d.reason && <p className="asst-route-why">{t('asst.route.why', { reason: d.reason })}</p>}
      {d.fixed && <p className="asst-route-why">{t(d.fixed.mode === 'new' ? 'asst.route.fixed_new' : 'asst.route.fixed_into')}</p>}
      {fixing && (
        <div className="asst-route-fix">
          {d.kind === 'send' && (
            <Button className="btn-sm" variant="secondary" busy={busy} onClick={() => fix('new')}>
              {t('asst.route.to_new')}
            </Button>
          )}
          <span className="asst-route-into">
            <Select value={target} onChange={setTarget} options={choices} placeholder={t('asst.route.pick')} aria-label={t('asst.route.pick')} />
            <Button className="btn-sm" variant="secondary" busy={busy} disabled={!target} onClick={() => fix('into')}>
              {t('asst.route.to_this')}
            </Button>
          </span>
          <button type="button" className="btn btn-plain btn-sm" onClick={() => setFixing(false)}>
            {t('action.cancel')}
          </button>
          <p className="muted asst-route-why">{t(d.kind === 'start' ? 'asst.route.fix_note_start' : 'asst.route.fix_note_send')}</p>
        </div>
      )}
    </div>
  );
}

/** 飞书机器人（docs/FEISHU_BOT.md）：私聊、群里 @ 它就是跟助理说话；下面是飞书 CLI 一步步装好 */
function FeishuBot({ state, onChange }) {
  const { t } = useTranslation();
  const { busy, save } = useSaveSettings(onChange);
  const f = state.feishu ?? {};
  const on = state.settings?.feishu !== false;
  return (
    <section>
      <h3 className="asst-h">
        <Send size={15} aria-hidden="true" />
        {t('asst.feishu.title')}
      </h3>
      {!f.bound ? (
        <p className="muted asst-small">
          {t(f.configured ? 'asst.feishu.unbound' : 'asst.feishu.none')} <Link to="/ai#remote">{t('asst.feishu.setup')}</Link>
        </p>
      ) : (
        <>
          <div className="asst-switch">
            <span>{t('asst.feishu.dm')}</span>
            <Switch checked={on} disabled={busy} label={t('asst.feishu.dm')} onChange={(v) => save({ feishu: v }, () => t(v ? 'asst.feishu.on' : 'asst.feishu.off'))} />
          </div>
          <p className="muted asst-small">{t(on ? 'asst.feishu.on_hint' : 'asst.feishu.off_hint')}</p>
          {on && !f.connected && <Note kind="warn">{t('asst.feishu.offline')}</Note>}
          {on && <LarkSteps initial={state.lark} />}
        </>
      )}
    </section>
  );
}

/** 飞书 CLI：看看只管装没装、用官方方式装；配应用、登录、怎么用由助理照官方技能做。装的时候每 3 秒看一次 */
function LarkSteps({ initial }) {
  const { t } = useTranslation();
  const toast = useToast();
  const [st, setSt] = useState(initial);
  const [busy, setBusy] = useState(false);
  useEffect(() => setSt(initial), [initial?.state, initial?.running]);
  const waiting = Boolean(st?.running);
  useEffect(() => {
    if (!waiting) return undefined;
    const iv = setInterval(() => api.get('/assistant/lark').then(setSt, () => {}), 3000);
    return () => clearInterval(iv);
  }, [waiting]);
  if (!st) return null;
  const install = () => {
    setBusy(true);
    api
      .post('/assistant/lark')
      .then(setSt, (e) => toast(e?.code === 'LARK_NEED_NODE' ? t('asst.feishu.lark_need_node') : errorText(t, e), 'error'))
      .finally(() => setBusy(false));
  };
  const ready = st.state === 'ready';
  return (
    <div className="asst-lark">
      <div className="asst-switch">
        <b>{t('asst.feishu.lark_title')}</b>
        <span className={`badge ${ready ? 'badge-ok' : ''}`}>{t(ready ? 'asst.feishu.lark_ready' : 'asst.feishu.lark_missing')}</span>
      </div>
      {ready ? (
        <p className="muted asst-small">{t('asst.feishu.lark_ready_hint')}</p>
      ) : (
        <>
          <p className="muted asst-small">{t('asst.feishu.lark_intro')}</p>
          {st.cli && !st.skills && <p className="asst-small">{t('asst.feishu.lark_no_skills')}</p>}
          {st.error && <Note kind="warn">{t('asst.feishu.lark_failed', { msg: st.error })}</Note>}
          {st.running ? (
            <p className="muted asst-small">
              <Loader2 size={13} className="spin" aria-hidden="true" /> {t('asst.feishu.lark_installing')}
            </p>
          ) : (
            <div className="nt-actions">
              <Button className="btn-sm" busy={busy} disabled={!st.npm && !st.auto_node} onClick={install}>
                {t('asst.feishu.lark_install')}
              </Button>
              <CopyButton text={st.command} label={t('asst.feishu.lark_copy')} />
            </div>
          )}
          <code className="mono asst-small ellipsis">{st.command}</code>
          {!st.npm && !st.auto_node && <p className="muted asst-small">{t('asst.feishu.lark_need_node')}</p>}
          <p className="muted asst-small">{t('asst.feishu.lark_also')}</p>
        </>
      )}
    </div>
  );
}

/** 约定：用户说过的、带条件或期限的要求（每一轮都带给助理），做完了可以划掉 */
function AgreementsTab({ state, onChange }) {
  const { t } = useTranslation();
  const toast = useToast();
  const drop = (id) => api.post('/assistant/agreements/remove', { id }).then(onChange, (e) => toast(errorText(t, e), 'error'));
  return (
    <section>
      <p className="muted asst-small">{t('asst.agreements_hint')}</p>
      {state.agreements.length === 0 ? (
        <p className="muted asst-small asst-empty">{t('asst.agreements_empty')}</p>
      ) : (
        <ul className="asst-agree">
          {state.agreements.map((a) => (
            <li key={a.id}>
              <ListChecks size={14} aria-hidden="true" className="asst-agree-ico" />
              <span>
                {a.text}
                {a.until && <span className="muted"> · {t('asst.until', { until: a.until })}</span>}
              </span>
              <button type="button" className="btn btn-plain btn-icon btn-sm" onClick={() => drop(a.id)} aria-label={t('asst.agreement_done')} title={t('asst.agreement_done')}>
                <X size={14} aria-hidden="true" />
              </button>
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}

/** 设置：派出去的会话的权限、重启、飞书 */
/** 助理自己动手时靠什么挡住（gateway/assistant.rs `protect`）：Codex 是它自己的沙箱；Claude Code 是看看的检查，只读时另开系统沙箱，如实写开没开起来 */
function Protect({ protect }) {
  const { t } = useTranslation();
  if (!protect) return null;
  const sb = protect.sandbox;
  let text;
  if (protect.engine === 'codex') text = t('asst.protect.codex');
  else if (protect.mode !== 'read_only') text = t('asst.protect.claude_gate');
  else if (!sb) text = t('asst.protect.claude_ro_unknown');
  else if (!sb.requested) text = t('asst.protect.claude_ro_pending');
  else if (sb.active) text = t('asst.protect.claude_ro_on');
  else text = t('asst.protect.claude_ro_off', { reason: sb.reason || t('asst.protect.unknown_reason') });
  return (
    <section>
      <h3 className="asst-h asst-h-sm">
        <ShieldCheck size={14} aria-hidden="true" />
        {t('asst.protect.title')}
      </h3>
      <p className="muted asst-small">{text}</p>
    </section>
  );
}

function SettingsTab({ state, onChange }) {
  const { t } = useTranslation();
  return (
    <>
      <section>
        <h3 className="asst-h asst-h-sm">
          <Settings2 size={14} aria-hidden="true" />
          {t('asst.tab.settings')}
        </h3>
        <div className="asst-switch">
          <span>{t('asst.restart')}</span>
          <RestartButton onDone={onChange} />
        </div>
        <p className="muted asst-small">{t('asst.restart_hint')}</p>
      </section>
      <Protect protect={state.protect} />
      <DispatchDefaults state={state} onChange={onChange} />
      <FeishuBot state={state} onChange={onChange} />
      <p className="muted asst-small asst-foot">
        <ChevronRight size={12} aria-hidden="true" />
        {t('asst.foot')}
      </p>
    </>
  );
}
