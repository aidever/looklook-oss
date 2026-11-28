// 看看客户端管理台。未登录时只显示登录页；登录后左侧栏（手机为抽屉）：用户菜单、新会话、工具（文件、SSH 主机、本机网页、
// AI、系统、设置，可收成一行图标）、项目和会话。电脑上侧栏能整条收成图标栏。
// 账户的事（电脑、会员与邀请、账户与安全）也在这里，放在用户菜单里，数据来自看看平台（shared/platform.js）。
// 本机 App 和浏览器远程打开是同一个界面，差别只在顶栏的连接胶囊（ConnChip）。
// 首页 / 是开始页（手机上是会话列表），/s/{id} 在右边打开会话；整页终端 /t/{id} 占满整个窗口（手机、新窗口）。
// 界面不出现“ttyd、tmux、隧道、端口转发”等术语。
import { Activity, Bot, CalendarClock, ChevronsUpDown, Copy, FolderOpen, Globe, History, Languages, Laptop, ListTodo, LogIn, LogOut, MessageSquare, Plus, Puzzle, Server, Settings as SettingsIcon, ShieldCheck, Sparkles, UserRound, Wrench } from 'lucide-react';
import { createContext, Fragment, useCallback, useContext, useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';

import Account from './pages/Account.jsx';
import Board, { useAttnCount } from './pages/Board.jsx';
import AiPage from './pages/Ai.jsx';
import Assistant from './pages/Assistant.jsx';
import Computers from './pages/Computers.jsx';
import Files from './pages/Files.jsx';
import Login from './pages/Login.jsx';
import Membership from './pages/Membership.jsx';
import Pages from './pages/Pages.jsx';
import Plugins from './pages/Plugins.jsx';
import PluginView from './pages/PluginView.jsx';
import { PLUGIN_ROUTE, parsePluginRoute, pickLocalized, pluginIcon, pluginRoute } from './shared/plugins.jsx';
import Settings from './pages/Settings.jsx';
import SshHosts from './pages/SshHosts.jsx';
import System from './pages/System.jsx';
import { HistoryPage } from './pages/History.jsx';
import { ProjectPage } from './pages/Project.jsx';
import PullRefresh from './shared/pull.jsx';
import ConnStatus from './shared/ConnStatus.jsx';
import { claimConn } from './shared/conn.js';
import { useGlobalShortcuts } from './shared/shortcuts.js';
import { hasSnapshot, rememberStatus, snapStatus } from './shared/snapshot.js';
import { opensInPage, SessionHome, SessionList, SessionStage, SessionTabs, SessionUiProvider, useSessionUi } from './pages/Sessions.jsx';
import { DraftPane } from './pages/Draft.jsx';
import TerminalView from './pages/TerminalView.jsx';
import ViewPage from './files/ViewPage.jsx';
import { api, localApi, rtcMode, selectedDevice } from './shared/api.js';
import { setCloudOperator } from './shared/operator.js';
import { useAwayTracker } from './shared/board.js';
import { DeviceSwitcher, DevicesProvider, deviceName, useDevices, useDeviceState } from './shared/devices.jsx';
import ErrorBoundary from './shared/ErrorBoundary.jsx';
import { Link, Router, useCanonicalPath, useRouter } from './shared/router.jsx';
import PopMenu from './shared/PopMenu.jsx';
import { sessionState, SessionsProvider, useSessions } from './shared/sessions.jsx';
import { daysLeft, formatTime } from './shared/time.js';
import { BrandMark, Button, Drawer, FoldNote, LangSwitch, Note, Skeleton, useConfirm, useToast } from './shared/ui.jsx';
import { isMod, keyLabel } from './shared/hotkeys.js';
import i18next, { setLocale } from './shared/i18n.js';
import QuickSwitch from './shared/QuickSwitch.jsx';
import ShortcutsHelp from './shared/Shortcuts.jsx';
import { errorText } from './shared/i18n.js';
import { ReportProvider, useReportDialog } from './shared/Report.jsx';
import { TipsProvider, useTips } from './shared/tips.jsx';
import { OnboardTracker } from './pages/Onboarding.jsx';
import { RelayHint } from './shared/RelayPicker.jsx';
import { ConnChip } from './shared/RelayStatus.jsx';
import { JustUpdatedBanner, shouldRemind, UpdateBanner, UpdateProvider } from './shared/Update.jsx';
import { TransferBanner } from './pages/SshTransfer.jsx';
import { remoteAllowed, renewSoon } from './shared/Remote.jsx';
import { splitTitle } from './shared/title.js';
import { ConnFailAlerts, InAppAlerts, useForegroundPing } from './shared/inapp.jsx';
import { noteVersion } from './shared/stale.js';

const StatusCtx = createContext({ status: null, reload: () => {} });

/** 客户端状态（账户、能否使用、终端方式、设置），每 10 秒刷新一次。 */
export function useStatus() {
  return useContext(StatusCtx);
}

/** 看看网页上的页面地址，例如续费、注册。 */
export function webUrl(status, path) {
  return `${(status?.account?.server ?? '').replace(/\/$/, '')}${path}`;
}

/** 安全提醒：白名单允许了外部地址却没开访问码时提醒开启访问码；局域网直连默认免输访问码不打扰首页。 */
export function SafetyNote({ full = false }) {
  const { t } = useTranslation();
  const { status } = useStatus();
  const c = status?.console;
  if (!c) return null;
  const open = c.allow_lan || c.allowed_ips > 0;
  const risky = c.allowed_ips > 0 && !c.code_enabled;
  const state = risky ? 'open_no_code' : open ? (c.code_enabled ? 'open' : 'lan_no_code') : 'local_only';
  if (!full) {
    if (!risky) return null;
    return (
      <FoldNote kind="warn" summary={`${t('safety.title')} · ${t(`safety.short.${state}`)}`}>
        <p>{t(`safety.${state}`)}</p>
        <p>{t('safety.body', { port: c.port })}</p>
      </FoldNote>
    );
  }
  // 只有这台电脑能打开：没什么要提醒的
  if (!open) return null;
  return (
    <Note kind={risky ? 'warn' : 'info'}>
      <b>{t('safety.title')}</b>
      <span style={{ display: 'block' }}>{t(`safety.${state}`)}</span>
    </Note>
  );
}

// 导航分两组（docs/DONT_MAKE_ME_THINK_2026-11.md §2）：上面是“在电脑上做事”的工具，下面是“配置”（AI 助手、设置）。
// 每个入口都带字（short），不让人悬停去猜图标。账户在用户菜单里。
const TOOLS = [
  { to: '/files', icon: FolderOpen, key: 'nav.files', short: 'nav.short.files' },
  { to: '/ssh', icon: Server, key: 'nav.ssh', short: 'nav.short.ssh' },
  { to: '/pages', icon: Globe, key: 'nav.pages', short: 'nav.short.pages' },
  { to: '/system', icon: Activity, key: 'nav.system', short: 'nav.short.system' },
  { to: '/ai', icon: Bot, key: 'nav.ai', short: 'nav.short.ai', config: true },
  { to: '/settings', icon: SettingsIcon, key: 'nav.settings', short: 'nav.short.settings', config: true },
];
// 按电脑的页面：多台电脑时顶栏显示电脑切换（会话合并显示所有电脑，账户页不分电脑）
const isDevicePage = (p) => p === '/assistant' || PLUGIN_ROUTE.test(p);
const TERM = /^\/t\/([a-z2-7]{8})$/;
// 新标签页里看一个文件（files/ViewPage.jsx）
const VIEW = '/view';
const SESS = /^\/s\/([a-z2-7]{8})$/;
// 用户菜单里的账户页面
const ACCOUNT_PAGES = ['/devices', '/membership', '/account'];
// 从右边滑出来的抽屉（地址照旧，链接、刷新都能直接打开）：盖在会话上面，不换掉它，关掉回到会话。
// 会话为主（docs/SESSION_FIRST_DESIGN_2026-11-16.md P0）：文件、SSH、网页、系统、AI 这些工具也是抽屉（更宽一档）
const TOOL_DRAWERS = ['/files', '/ssh', '/pages', '/system', '/ai'];
const DRAWER_PAGES = {
  '/settings': 'nav.settings',
  '/devices': 'acct.computers',
  '/account': 'acct.account',
  '/membership': 'acct.membership',
  ...Object.fromEntries(TOOLS.filter((x) => TOOL_DRAWERS.includes(x.to)).map((x) => [x.to, x.key])),
};
const isDrawer = (p) => Object.hasOwn(DRAWER_PAGES, p);
const PAGES = ['/', '/new', '/assistant', '/tasks', '/history', '/project', ...ACCOUNT_PAGES, ...TOOLS.map((x) => x.to)];
const isKnown = (p) => PAGES.includes(p) || p === VIEW || TERM.test(p) || SESS.test(p) || PLUGIN_ROUTE.test(p);
// 会话相关的页面铺满主区域（不限宽度）
const isWide = (p) => p === '/' || p === '/new' || p === '/assistant' || p === '/tasks' || p === '/history' || SESS.test(p) || PLUGIN_ROUTE.test(p);
const RAIL_KEY = 'looklook.rail';

function readFlag(key, fallback) {
  try {
    const v = localStorage.getItem(key);
    return v === null ? fallback : v === '1';
  } catch {
    return fallback;
  }
}
function writeFlag(key, v) {
  try {
    localStorage.setItem(key, v ? '1' : '0');
  } catch {
    /* 无痕模式等：只是不记住 */
  }
}

export function Initial({ name, large }) {
  if (name === null) {
    return (
      <span className={`avatar avatar-guest ${large ? 'avatar-lg' : ''}`}>
        <UserRound size={large ? 22 : 16} aria-hidden="true" />
      </span>
    );
  }
  return <span className={`avatar ${large ? 'avatar-lg' : ''}`}>{(name || '?').trim().slice(0, 1).toUpperCase()}</span>;
}

/** 确认后退出登录；连不上服务器时询问是否只退出这台电脑。 */
export function useLogout() {
  const { t } = useTranslation();
  const { reload } = useStatus();
  const confirm = useConfirm();
  const toast = useToast();
  const devices = useDevices();
  return async () => {
    // 多台电脑时，退出的是当前选中的那台，说清楚是哪台
    const body = devices.multi ? t('devices.logout_body', { name: deviceName(t, devices.byId(devices.selected)) }) : t('logout.body');
    if (!(await confirm({ title: t('logout.title'), body, confirm: t('logout.confirm'), danger: true }))) return;
    try {
      await api.post('/auth/logout', { force: false });
    } catch (e) {
      if (e.code !== 'NETWORK') {
        toast(errorText(t, e), 'error');
        return;
      }
      if (!(await confirm({ title: t('logout.offline_title'), body: t('logout.offline_body'), confirm: t('logout.offline_confirm'), danger: true }))) return;
      await api.post('/auth/logout', { force: true });
    }
    reload();
  };
}

const RENEW_LATER_KEY = 'looklook.renew_later';

/** 稍后提醒：记下到期时间和当时还剩几天，剩 3 天、1 天时再提醒一次。 */
function useRenewLater(expires, left) {
  const tier = left === null ? null : left <= 1 ? 1 : left <= 3 ? 3 : 14;
  const key = `${expires}:${tier}`;
  const [later, setLater] = useState(() => {
    try {
      return localStorage.getItem(RENEW_LATER_KEY);
    } catch {
      return null;
    }
  });
  const dismiss = () => {
    try {
      localStorage.setItem(RENEW_LATER_KEY, key);
    } catch {
      /* 只是不记住 */
    }
    setLater(key);
  };
  return [later === key, dismiss];
}

function GateBanner() {
  const { t } = useTranslation();
  const { status, reload } = useStatus();
  const acc = status.account;
  const soon = renewSoon(status);
  const [later, dismiss] = useRenewLater(acc.session?.membership_expires_at, soon);
  const reconnect = async () => {
    await api.post('/auth/reconnect').catch(() => {});
    setTimeout(reload, 1500);
  };
  // 没登录：这台电脑照常用，远程访问的入口在顶栏、用户菜单和本机网页里，不在这里打扰
  if (!acc.logged_in) return null;
  if (!remoteAllowed(status)) {
    const expired = acc.reason === 'MEMBERSHIP_EXPIRED';
    return (
      <div className="banner banner-error" role="alert">
        <span className="grow">{t(`gate.${acc.reason}`, { defaultValue: t('gate.other') })}</span>
        {expired && (
          <Link to="/membership" className="btn btn-danger">
            {t('gate.renew')}
          </Link>
        )}
        {acc.reason === 'OFFLINE_TOO_LONG' && (
          <Button variant="danger" onClick={reconnect}>
            {t('gate.reconnect')}
          </Button>
        )}
      </div>
    );
  }
  if (soon !== null && !later) {
    return (
      <div className="banner banner-warn">
        <span className="grow">{t('upsell.renew_soon', { count: soon })}</span>
        <Link to="/membership" className="btn btn-primary">
          {t('gate.renew')}
        </Link>
        <Button variant="ghost" onClick={dismiss}>
          {t('upsell.renew_later')}
        </Button>
      </div>
    );
  }
  if (acc.last_error) {
    // 心跳失败不一定是网络不通：服务端报错时给出错误码；远程通道仍连着时只是账户状态没同步上，别和右上角“远程在线”打架。
    const deadline = formatTime(acc.session?.offline_deadline);
    const text =
      acc.last_error !== 'NETWORK'
        ? t('gate.unstable_server', { code: acc.last_error, deadline })
        : status.relay?.state === 'connected'
          ? t('gate.unstable_sync', { deadline })
          : t('gate.unstable', { deadline });
    return (
      <div className="banner banner-warn">
        <span className="grow">{text}</span>
        <Button variant="ghost" onClick={reconnect}>
          {t('gate.reconnect')}
        </Button>
      </div>
    );
  }
  return null;
}

/** 用户菜单（图标栏的头像）：顶上一张头像卡片（名字、专属地址点一下复制；没登录时是开通远程），
 *  中间几行账户和设置，底下一行小字链接（语言、上手指南、快捷键、报告问题、主页、文档、退出）。
 *  grid：“我的”菜单在卡片下面插一块图标格子（任务、历史、工具、AI、电脑），这时电脑就不在列表里了。
 *  外部链接在 App 里由系统浏览器打开（desktop.rs 拦截新窗口）。 */
function useUserMenu(grid) {
  const { t, i18n } = useTranslation();
  const { status } = useStatus();
  const { navigate } = useRouter();
  const logout = useLogout();
  const openReport = useReportDialog();
  const toast = useToast();
  const tips = useTips();
  const open = (url) => () => window.open(url, '_blank', 'noopener');
  const left = daysLeft(status.account.session?.membership_expires_at);
  const en = i18n.language?.startsWith('en');
  const s = status.account.session;
  const guest = !status.account.logged_in;
  // 专属地址：手机、外面的电脑打开它就能用；卡片上一直看得到，点一下复制
  const addr = s?.user_host_url;
  const copyAddr = async () => {
    try {
      await navigator.clipboard.writeText(addr);
      toast(t('acct.address_copied'));
    } catch {
      window.prompt('', addr);
    }
  };
  const fine = typeof window !== 'undefined' && window.matchMedia?.('(pointer: fine)').matches;
  const card = (close) => (
    <div className="me-card">
      <Initial name={s ? s.user.nickname : null} />
      <div className="me-card-text">
        {guest ? (
          <>
            <b className="ellipsis">{t('upsell.menu')}</b>
            <span className="me-card-sub">{t('upsell.menu_hint')}</span>
          </>
        ) : (
          <>
            <span className="me-card-name">
              <b className="ellipsis">{s?.user.nickname}</b>
              <button type="button" className={`me-card-pill ${left > 0 ? '' : 'expired'}`} onClick={(e) => (e.stopPropagation(), close(), navigate('/membership'))} title={t('acct.membership')}>
                <CalendarClock size={12} aria-hidden="true" />
                {left > 0 ? t('account.days_left', { n: left }) : t('account.expired')}
              </button>
            </span>
            {addr && (
              <button type="button" className="me-card-addr" title={t('acct.address')} onClick={(e) => (e.stopPropagation(), close(), copyAddr())}>
                <span className="ellipsis">{addr.replace(/^https?:\/\//, '').replace(/\/$/, '')}</span>
                <Copy size={13} aria-hidden="true" />
              </button>
            )}
          </>
        )}
      </div>
      {guest && (
        <button type="button" className="btn btn-sm btn-primary me-card-cta" onClick={(e) => (e.stopPropagation(), close(), navigate('/account'))}>
          <LogIn size={14} aria-hidden="true" />
          {t('upsell.cta_short')}
        </button>
      )}
    </div>
  );
  return [
    { card },
    ...(grid ? [{ grid }] : []),
    'sep',
    !grid && (!guest || !rtcMode) && { icon: Laptop, label: t('acct.computers'), onClick: () => navigate('/devices') },
    !guest && { icon: CalendarClock, label: t('acct.membership'), onClick: () => navigate('/membership') },
    !guest && { icon: ShieldCheck, label: t('acct.account'), onClick: () => navigate('/account') },
    { icon: SettingsIcon, label: t('nav.settings'), hint: shouldRemind(status.update) ? t('update.badge') : undefined, onClick: () => navigate('/settings') },
    {
      links: [
        { icon: Languages, label: en ? '中文' : 'English', onClick: () => setLocale(en ? 'zh-CN' : 'en-US') },
        { label: t('onboard.menu'), onClick: () => (tips.unmark('onboard.hide'), navigate('/')) },
        fine && { label: t('keys.title'), title: `${t('keys.title')} (${keyLabel('mod+/')})`, onClick: () => window.dispatchEvent(new Event('ll-shortcuts')) },
        // 问题报告经看看服务器转交，要登录
        !guest && { label: t('report.title'), onClick: openReport },
        { label: t('nav.home_page'), onClick: open(webUrl(status, '/')) },
        { label: t('nav.docs'), onClick: open(webUrl(status, '/docs')) },
        !guest && { icon: LogOut, label: t('nav.logout'), onClick: logout, danger: true },
      ],
    },
  ];
}

/** 标签页标题前加上等你处理的个数：`(2) 看看`。每次重画都对一遍（对话页会临时在标题前加 ●，之后还原） */
function useTitleCount() {
  const n = useAttnCount();
  useEffect(() => {
    const { dot, base } = splitTitle(document.title);
    const want = `${dot ? '● ' : ''}${n > 0 ? `(${n}) ` : ''}${base}`;
    if (document.title !== want) document.title = want;
  });
}

// 会话栏只在会话相关的页面出现（docs/UI_REDESIGN_2026-10.md §2.1）；工具页内容区铺满
const isSessionArea = (p) => p === '/' || p === '/project' || SESS.test(p);
const PANE_W_KEY = 'looklook.panew';
const PANE_MIN = 240;
const PANE_MAX = 440;

function readWidth() {
  try {
    const v = Number(localStorage.getItem(PANE_W_KEY));
    return v >= PANE_MIN && v <= PANE_MAX ? v : 288;
  } catch {
    return 288;
  }
}

/** 图标栏上的一个入口：图标下面一直带着短名字（short），悬停显示全名和快捷键，右上角可以带个数或红点。 */
function RailItem({ to, onClick, icon: Icon, label, short, hint, active, count, dot, className = '', disabled }) {
  const title = hint ? `${label} (${hint})` : label;
  const inner = (
    <>
      <span className="rail-ico">
        <Icon size={19} aria-hidden="true" />
        {count > 0 && <span className="rail-count">{count}</span>}
        {dot && <span className="dot-badge" aria-hidden="true" />}
      </span>
      <span className="rail-label">{short ?? label}</span>
    </>
  );
  const cls = `rail-btn ${active ? 'active' : ''} ${className}`;
  if (to) {
    return (
      <Link to={to} className={cls} aria-label={label} title={title} aria-current={active ? 'page' : undefined}>
        {inner}
      </Link>
    );
  }
  return (
    <button type="button" className={cls} onClick={onClick} aria-label={label} title={title} disabled={disabled} aria-pressed={active}>
      {inner}
    </button>
  );
}

/**
 * 会话栏收起时最左边的窄图标栏：logo、助理、新会话、展开会话栏、任务、历史；底部是工具（抽屉）、AI、设置、连接状态、用户菜单。
 */
function NavRail({ path, onPane }) {
  const { t } = useTranslation();
  const { status } = useStatus();
  const ui = useSessionUi();
  const userMenu = useUserMenu();
  const n = useAttnCount();
  const s = status.account.session;
  const toolMenu = useToolMenu();
  const asst = useAssistantSummary();
  return (
    <nav className="rail" aria-label={t('nav.main')}>
      <Link to="/" className="rail-brand" aria-label={t('brand')} title={t('brand')}>
        <BrandMark size={30} />
      </Link>
      {/* 助理：比会话高一级，一个入口说话，它自己派活（docs/ASSISTANT.md） */}
      <RailItem to="/assistant" icon={Sparkles} label={t('asst.title')} short={t('nav.short.assistant')} active={path === '/assistant'} count={asst.waiting} dot={asst.thinking && !asst.waiting} />
      <RailItem onClick={() => ui.newSession()} icon={Plus} label={t('sess.new')} short={t('nav.short.new')} hint={keyLabel('mod+shift+O')} className="rail-new" disabled={!status.account.allowed} />
      <RailItem onClick={() => onPane(true)} icon={MessageSquare} label={t('sess.expand_side')} short={t('nav.short.sessions')} hint={keyLabel('mod+shift+S')} active={isSessionArea(path)} />
      <RailItem to="/tasks" icon={ListTodo} label={n > 0 ? t('board.nav_count', { count: n }) : t('board.title')} short={t('nav.short.tasks')} active={path === '/tasks'} count={n} />
      <RailItem to="/history" icon={History} label={t('hist.title')} short={t('nav.short.history')} active={path === '/history'} />
      <span className="rail-grow" aria-hidden="true" />
      <PopMenu items={toolMenu} className="rail-btn" label={t('sess.tools')}>
        <span className="rail-ico rail-live">
          <Wrench size={20} aria-hidden="true" />
          {(status.plugins ?? []).some((p) => p.running) && <span className="dot-badge" aria-hidden="true" />}
        </span>
        <span className="rail-label">{t('sess.tools')}</span>
      </PopMenu>
      {TOOLS.filter((x) => x.config).map((x) => (
        <RailItem key={x.to} to={x.to} icon={x.icon} label={t(x.key)} short={t(x.short)} dot={x.to === '/settings' && shouldRemind(status.update)} />
      ))}
      <span className="rail-conn">
        <ConnChip />
      </span>
      <PopMenu items={userMenu} className="rail-btn rail-me" label={s?.user.nickname ?? t('upsell.menu')}>
        <span className="rail-ico">
          <Initial name={s ? s.user.nickname : null} />
        </span>
        <span className="rail-label">{t('nav.me')}</span>
      </PopMenu>
    </nav>
  );
}

/** 装着的插件的入口（看看不认识具体插件，名字、图标来自插件清单）；跑着的带“运行中” */
function usePluginItems() {
  const { t, i18n } = useTranslation();
  const { navigate } = useRouter();
  const { status } = useStatus();
  return (status.plugins ?? [])
    .filter((p) => !p.incompatible)
    .map((p) => ({ icon: pluginIcon(p.icon), label: pickLocalized(p.title, i18n.language) || p.name, hint: p.running ? t('plugins.running') : undefined, onClick: () => navigate(pluginRoute(p.name)) }));
}

/** 工具菜单：文件、SSH、网页、系统 */
function useToolMenu() {
  const { t } = useTranslation();
  const { navigate } = useRouter();
  return TOOLS.filter((x) => !x.config).map((x) => ({ icon: x.icon, label: t(x.key), onClick: () => navigate(x.to) }));
}

/**
 * 助理一行要的数（`GET /api/assistant/summary`）：开没开、在不在想、派出去的活里等你的 / 在干的。看得见页面时几秒拉一次
 */
const ASST_POLL_MS = 5000;
const AsstCtx = createContext({ enabled: false, thinking: false, waiting: 0, working: 0, loaded: false });
function AssistantSummaryProvider({ children }) {
  const devices = useDevices();
  const [sum, setSum] = useState({ enabled: false, thinking: false, waiting: 0, working: 0, loaded: false });
  useEffect(() => {
    let live = true;
    const load = () =>
      document.visibilityState === 'visible' &&
      api.get('/assistant/summary').then(
        (d) => live && setSum({ ...d, loaded: true }),
        () => live && setSum((x) => ({ ...x, loaded: true })),
      );
    load();
    const id = setInterval(load, ASST_POLL_MS);
    return () => {
      live = false;
      clearInterval(id);
    };
  }, [devices.selected]);
  return <AsstCtx.Provider value={sum}>{children}</AsstCtx.Provider>;
}
const useAssistantSummary = () => useContext(AsstCtx);

/** 会话栏最上面固定的“助理”一行：比会话高一级（会话是它派活的地方），一眼看到它在干什么、有几件等你 */
function AssistantRow({ path }) {
  const { t } = useTranslation();
  const a = useAssistantSummary();
  const active = path === '/assistant';
  const sub = !a.enabled
    ? t('asst.row_off')
    : a.waiting > 0
      ? t('asst.row_waiting', { count: a.waiting })
      : a.thinking
        ? t('asst.row_thinking')
        : a.working > 0
          ? t('asst.row_working', { count: a.working })
          : t('asst.row_idle');
  return (
    <Link to="/assistant" className={`spane-asst ${active ? 'active' : ''} ${a.waiting > 0 ? 'attn' : ''}`} aria-current={active ? 'page' : undefined}>
      <span className="spane-asst-ico">
        <Sparkles size={17} aria-hidden="true" />
        {(a.thinking || a.working > 0) && <span className="spane-asst-live" aria-hidden="true" />}
      </span>
      <span className="spane-asst-text">
        <span className="spane-asst-name">{t('asst.title')}</span>
        <span className="spane-asst-sub">{sub}</span>
      </span>
      {a.waiting > 0 && <span className="spane-asst-count">{a.waiting}</span>}
    </Link>
  );
}

/** 会话栏顶上：看看 + 新会话，下面是固定的助理一行（会话为主：导航就是会话栏，docs/SESSION_FIRST_DESIGN_2026-11-16.md §3.1） */
function PaneNav({ path }) {
  const { t } = useTranslation();
  const { status } = useStatus();
  const ui = useSessionUi();
  return (
    <div className="spane-nav">
      <div className="spane-top">
        <Link to="/" className="spane-brand" aria-label={t('brand')} title={t('brand')}>
          <BrandMark size={26} />
          <span>{t('brand')}</span>
        </Link>
        <button type="button" className="btn btn-sm spane-new" onClick={() => ui.newSession()} disabled={!status.account.allowed} title={`${t('sess.new')} (${keyLabel('mod+shift+O')})`}>
          <Plus size={15} aria-hidden="true" />
          <span>{t('sess.new')}</span>
        </button>
      </div>
      <AssistantRow path={path} />
    </div>
  );
}

/** “我的”菜单：头像卡片下面一块图标格子（任务、历史、文件、SSH、网页、系统、AI、电脑），再是账户那些
 *  （手机底栏的“我的”、电脑会话栏底部的头像共用） */
function useMeMenu({ tasks = true } = {}) {
  const { t } = useTranslation();
  const { status } = useStatus();
  const { navigate } = useRouter();
  const n = useAttnCount();
  const item = (x) => ({ icon: x.icon, label: t(x.short), onClick: () => navigate(x.to) });
  return useUserMenu([
    tasks && { icon: ListTodo, label: t('nav.short.tasks'), count: n, onClick: () => navigate('/tasks') },
    { icon: History, label: t('nav.short.history'), onClick: () => navigate('/history') },
    ...TOOLS.filter((x) => !x.config).map(item),
    item(TOOLS.find((x) => x.to === '/ai')),
    (status.account.logged_in || !rtcMode) && { icon: Laptop, label: t('acct.computers'), onClick: () => navigate('/devices') },
  ]);
}

/** 会话栏底部一行：头像（点开是“我的”：任务、历史、工具、AI、账户）· 连接状态 · 设置 */
function PaneFoot() {
  const { t } = useTranslation();
  const { status } = useStatus();
  const { path } = useRouter();
  const menu = useMeMenu();
  const s = status.account.session;
  // 没登录时开通远程的入口就在旁边的连接胶囊上，这里只写“我的”
  const name = s?.user.nickname ?? t('nav.me');
  return (
    <div className="spane-foot">
      <PopMenu items={menu} className="btn btn-plain spane-user" label={name}>
        <Initial name={s ? s.user.nickname : null} />
        <span className="ellipsis">{name}</span>
        <ChevronsUpDown size={14} className="spane-user-more" aria-hidden="true" />
      </PopMenu>
      <ConnChip />
      <Link to="/settings" className={`btn btn-plain spane-gear ${path === '/settings' ? 'active' : ''}`} aria-label={t('nav.settings')} title={t('nav.settings')}>
        <SettingsIcon size={17} aria-hidden="true" />
        {shouldRemind(status.update) && <span className="dot-badge" aria-hidden="true" />}
      </Link>
    </div>
  );
}

/** 会话栏：项目和会话列表，右边缘可以拖动改宽度（记在本机）。 */
function SessionPane({ path, width, onWidth, onCollapse }) {
  const { t } = useTranslation();
  const current = path.match(SESS)?.[1];
  const drag = (e) => {
    if (e.button !== 0) return;
    e.preventDefault();
    const x0 = e.clientX;
    const w0 = width;
    let w = w0;
    const move = (ev) => {
      w = Math.max(PANE_MIN, Math.min(PANE_MAX, w0 + ev.clientX - x0));
      onWidth(w, false);
    };
    const up = () => {
      window.removeEventListener('pointermove', move);
      window.removeEventListener('pointerup', up);
      document.body.classList.remove('resizing');
      onWidth(w, true);
    };
    document.body.classList.add('resizing');
    window.addEventListener('pointermove', move);
    window.addEventListener('pointerup', up);
  };
  const key = (e) => {
    if (e.key !== 'ArrowLeft' && e.key !== 'ArrowRight') return;
    e.preventDefault();
    onWidth(Math.max(PANE_MIN, Math.min(PANE_MAX, width + (e.key === 'ArrowLeft' ? -16 : 16))), true);
  };
  return (
    <aside className="spane" id="side-nav" aria-label={t('sess.title')}>
      <PaneNav path={path} />
      <SessionList currentId={current} onCollapse={onCollapse} />
      <PaneFoot />
      {/* eslint-disable-next-line jsx-a11y/no-noninteractive-tabindex -- 分隔条可以用方向键调宽度 */}
      <div className="spane-resize" role="separator" aria-orientation="vertical" aria-valuenow={width} aria-valuemin={PANE_MIN} aria-valuemax={PANE_MAX} aria-label={t('sess.pane_resize')} tabIndex={0} onPointerDown={drag} onKeyDown={key} onDoubleClick={() => onWidth(288, true)} />
    </aside>
  );
}

/**
 * 手机底部的标签栏：助理 · 会话 · ＋新建 · 任务 · 我的（5 个，＋ 正好在中间）。助理比会话高一级，放第一个（数字 = 派出去的活里等你的）；
 * 会话、任务上的数字 = 等你处理的会话个数；历史、工具、AI 在“我的”最上面。
 */
function MobileTabs({ path }) {
  const { t } = useTranslation();
  const { status } = useStatus();
  const ui = useSessionUi();
  const menu = useMeMenu({ tasks: false });
  const n = useAttnCount();
  const asst = useAssistantSummary();
  const s = status.account.session;
  const mine = path === '/history' || PLUGIN_ROUTE.test(path);
  return (
    <nav className="mtabs mtabs-5" aria-label={t('nav.main')}>
      <Link to="/assistant" className={`mtab ${path === '/assistant' ? 'active' : ''}`}>
        <Sparkles size={21} aria-hidden="true" />
        {asst.waiting > 0 && <span className="rail-count">{asst.waiting}</span>}
        <span>{t('nav.short.assistant')}</span>
      </Link>
      <Link to="/" className={`mtab ${isSessionArea(path) ? 'active' : ''}`}>
        <MessageSquare size={21} aria-hidden="true" />
        {n > 0 && <span className="rail-count">{n}</span>}
        <span>{t('sess.title')}</span>
      </Link>
      <button type="button" className="mtab mtab-new" onClick={() => ui.newSession()} disabled={!status.account.allowed} aria-label={t('sess.new')}>
        <span className="mtab-plus">
          <Plus size={22} aria-hidden="true" />
        </span>
      </button>
      <Link to="/tasks" className={`mtab ${path === '/tasks' ? 'active' : ''}`}>
        <ListTodo size={21} aria-hidden="true" />
        <span>{t('nav.short.tasks')}</span>
      </Link>
      <PopMenu items={menu} className={`mtab ${mine ? 'active' : ''}`} label={t('nav.me')} sheet>
        <Initial name={s ? s.user.nickname : null} />
        {shouldRemind(status.update) && <span className="dot-badge" aria-hidden="true" />}
        <span>{t('nav.me')}</span>
      </PopMenu>
    </nav>
  );
}

/**
 * 打开看看时停在首页，不自动跳去助理（助理入口在会话栏最上面和手机底栏）。电脑上有最近打开过、还在的会话就回到它
 * （会话为主 P2），手机上就是会话列表。只在刚打开时做一次；之后点“新会话”来到首页照常是草稿。
 */
// 整个页面生命周期只做一次：手机上进终端页（/t/）会卸掉 ConsoleLayout，返回时重新挂载，不能再当成“刚打开”
let resumed = false;
function useResumeLast(path) {
  const sessions = useSessions();
  const ui = useSessionUi();
  const first = useRef(path);
  useEffect(() => {
    if (resumed || sessions.loading) return;
    resumed = true;
    if (first.current !== '/' || path !== '/' || window.location.search) return;
    if (!opensInPage()) return;
    const last = sessions.recent.find((id) => sessions.byId(id));
    if (last) ui.openSession(last);
  }, [sessions.loading, path]);
}

function ConsoleLayout({ path, stale, children }) {
  const { t } = useTranslation();
  const { status } = useStatus();
  const devices = useDevices();
  const ui = useSessionUi();
  const { navigate } = useRouter();
  const switcher = (devices.multi || devices.localOnly) && isDevicePage(path);
  const [paneOpen, setPaneState] = useState(() => !readFlag(RAIL_KEY, false));
  const setPane = (v) => {
    writeFlag(RAIL_KEY, !v);
    setPaneState(v);
  };
  const [width, setWidth] = useState(readWidth);
  const onWidth = (w, save) => {
    setWidth(w);
    if (save) {
      try {
        localStorage.setItem(PANE_W_KEY, String(w));
      } catch {
        /* 无痕模式等：只是不记住 */
      }
    }
  };
  const paneRef = useRef(paneOpen);
  paneRef.current = paneOpen;
  const pathRef = useRef(path);
  pathRef.current = path;
  // Ctrl+Shift+O 新会话，Ctrl+Shift+S 收起 / 展开会话栏（焦点在终端里时由终端自己处理按键）。
  useEffect(() => {
    const onKey = (e) => {
      if (!(e.ctrlKey || e.metaKey) || !e.shiftKey || e.altKey) return;
      const k = e.key.toLowerCase();
      if (k === 'o') {
        e.preventDefault();
        ui.newSession();
      } else if (k === 's' && window.innerWidth >= 900) {
        e.preventDefault();
        setPane(!paneRef.current);
      }
    };
    // ⌘, / Ctrl+, 打开设置（Mac 桌面版由原生菜单发 `ll-menu`，见 src/desktop.rs 的 page_command）
    const onSettings = (e) => {
      if (!isMod(e, ',')) return;
      e.preventDefault();
      navigate('/settings');
    };
    const onMenu = (e) => {
      if (e.detail === 'settings') navigate('/settings');
      else if (e.detail === 'pet_settings') navigate('/ai#pet');
      else if (e.detail === 'new_session') ui.newSession();
    };
    window.addEventListener('keydown', onKey);
    window.addEventListener('keydown', onSettings);
    window.addEventListener('ll-menu', onMenu);
    return () => {
      window.removeEventListener('keydown', onKey);
      window.removeEventListener('keydown', onSettings);
      window.removeEventListener('ll-menu', onMenu);
    };
  }, [ui]);
  const wide = isWide(path);
  const sess = path.match(SESS)?.[1] ?? null;
  // 会话栏在哪一页都在（会话为主）；收起时是窄的图标栏
  const pane = paneOpen;
  useTitleCount();
  useForegroundPing();
  useResumeLast(path);
  return (
    <>
      {/* 右上角的提醒：连不上的电脑（要手动关）在上，会话的提醒在下 */}
      <div className="notices" aria-live="polite">
        <ConnFailAlerts />
        <InAppAlerts activeId={sess} open={ui.openSession} />
      </div>
      {/* 手机上的顶栏；电脑上这些都在图标栏里 */}
      <header className="topbar topbar-console">
        <Link to="/" className="brand">
          <BrandMark />
          <span className="brand-text">{t('brand')}</span>
        </Link>
        <span className="inline" style={{ flexWrap: 'nowrap' }}>
          {/* 助理在底栏第一个（手机）；电脑上是会话栏最上面那一行 */}
          {switcher && <DeviceSwitcher perDevice helpUrl={webUrl(status, '/docs/remote')} />}
          <ConnChip />
        </span>
      </header>
      <div className={`console ${pane ? 'console-pane' : ''} ${sess ? 'console-sess' : ''} ${path === '/assistant' || PLUGIN_ROUTE.test(path) ? 'console-full' : ''}`} style={{ '--pane-w': `${width}px` }}>
        {/* 展开时只有会话栏（导航在它的头尾）；收起时是窄图标栏 */}
        {pane ? <SessionPane path={path} width={width} onWidth={onWidth} onCollapse={() => setPane(false)} /> : <NavRail path={path} onPane={setPane} />}
        <div className="console-col">
          {/* 提示横幅放在内容区上面：图标栏和会话栏从窗口顶上一直到底 */}
          <GateBanner />
          <TransferBanner />
          <UpdateBanner />
          <JustUpdatedBanner />
          <RelayHint />
        <main className={`console-main ${wide ? 'console-wide' : 'shell'} ${sess ? 'console-session' : ''}`}>
          {/* 打开的会话排成顶部标签（电脑上，开始页和会话页） */}
          {(sess || path === '/') && <SessionTabs activeId={sess} />}
          {/* 刚切换电脑、新电脑的状态还没拿到时先不渲染页面，免得页面用上一台电脑的设置初始化 */}
          <Fragment key={switcher ? devices.selected : 'all'}>
            {switcher && stale ? (
              <div className="stack">
                <Skeleton height={60} />
                <Skeleton height={180} />
              </div>
            ) : (
              <ErrorBoundary resetKey={path}>{children}</ErrorBoundary>
            )}
          </Fragment>
          {/* 打开过的会话一直挂着（不在会话页时隐藏），切回来不用重新连 */}
          <SessionStage activeId={sess} />
        </main>
        </div>
      </div>
      <MobileTabs path={path} />
      <QuickSwitch />
      <ShortcutsHelp />
    </>
  );
}

/** 设置、电脑、账户、会员的抽屉。设置按电脑分：多台电脑时标题旁边能切换，换电脑时等那台的状态到了再显示 */
function PageDrawer({ path, behind, onClose, stale, children }) {
  const { t } = useTranslation();
  const { status } = useStatus();
  const devices = useDevices();
  const sessions = useSessions();
  const perDevice = path === '/settings' && (devices.multi || devices.localOnly);
  // 工具页（AI 工具、文件、SSH、本地页面、系统）也是按电脑的：页头自己带切换器，这里换电脑时整页重挂、等新状态
  const byDevice = (perDevice || TOOL_DRAWERS.includes(path)) && (devices.multi || devices.localOnly);
  const close = () => {
    onClose();
    // 回到会话：焦点交还给它的输入框
    const id = behind.match(SESS)?.[1];
    if (id) setTimeout(() => sessions.requestFocus(id), 0);
  };
  return (
    <Drawer open={Boolean(path)} onClose={close} title={!path ? '' : !status.account.logged_in && ACCOUNT_PAGES.includes(path) && path !== '/devices' ? t('upsell.menu') : t(DRAWER_PAGES[path])} wide={TOOL_DRAWERS.includes(path) ? 'xl' : path === '/settings'} head={perDevice && <DeviceSwitcher perDevice helpUrl={webUrl(status, '/docs/remote')} />}>
      <Fragment key={byDevice ? devices.selected : 'all'}>
        {byDevice && stale ? (
          <div className="stack">
            <Skeleton height={60} />
            <Skeleton height={180} />
          </div>
        ) : (
          <ErrorBoundary resetKey={path}>{children}</ErrorBoundary>
        )}
      </Fragment>
    </Drawer>
  );
}

function PublicLayout({ children }) {
  const { t } = useTranslation();
  return (
    <>
      <header className="topbar">
        <span className="brand">
          <BrandMark />
          {t('brand')}
        </span>
        <LangSwitch />
      </header>
      <main className="shell">{children}</main>
    </>
  );
}

function Unreachable({ onRetry }) {
  const { t } = useTranslation();
  return (
    <PublicLayout>
      <div className="auth">
        <Note kind="error">
          <b>{t('unreachable.title')}</b>
          <br />
          {t('unreachable.body')}
        </Note>
        <Button block onClick={onRetry}>
          {t('unreachable.retry')}
        </Button>
      </div>
    </PublicLayout>
  );
}

/**
 * 抽屉页面下面露出来的是哪一页：打开抽屉之前所在的那页（一般是会话）；直接打开抽屉的地址时是首页。
 * 关掉：从那页打开的就后退回去（浏览器的后退、手机的返回手势也一样关掉）；直接打开的换成那页的地址。
 */
function useDrawerRoute(path) {
  const { navigate, pop, replace } = useRouter();
  const behind = useRef(isDrawer(path) ? '/' : path);
  // 从下面那页打开抽屉以来压进历史的条数（在抽屉里点链接到别的抽屉页也算）：关掉时后退这么多步
  const pushed = useRef(0);
  const prev = useRef(path);
  if (!isDrawer(path)) behind.current = path;
  useEffect(() => {
    if (prev.current === path) return;
    if (!isDrawer(path)) pushed.current = 0;
    else if (pop) pushed.current = Math.max(0, pushed.current - 1);
    else if (!replace) pushed.current += 1;
    prev.current = path;
  }, [path]);
  const close = () => {
    const n = pushed.current;
    pushed.current = 0;
    if (n > 0 && window.history.length > n) window.history.go(-n);
    else navigate(behind.current, { replace: true });
  };
  return { drawer: isDrawer(path) ? path : null, behind: behind.current, close };
}

function Shell() {
  const path0 = useCanonicalPath(isKnown);
  const route = useDrawerRoute(path0);
  // 抽屉开着时，主区域显示它下面的那页（会话不卸载、不隐藏）
  const path = route.drawer ? route.behind : path0;
  // 远程打开时先按上次的快照显示（docs/REMOTE_UX_2026-11-07.md §3.4），连上电脑后静默换成最新的
  const [status, setStatus] = useState(snapStatus);
  const toastTop = useToast();
  useGlobalShortcuts();
  const toastRef = useRef(toastTop);
  toastRef.current = toastTop;
  const [down, setDown] = useState(false);
  // 告诉启动页状态由管理台显示（它收起骨架、藏起自己的提示条）
  useEffect(() => claimConn(hasSnapshot()), []);
  useAwayTracker();
  // 状态来自哪台电脑（多设备时随顶栏的选择）
  const [statusDevice, setStatusDevice] = useState(null);
  const statusDevRef = useRef(null);
  statusDevRef.current = statusDevice;
  const devRef = useRef(null);
  const relayState = status?.relay?.state;
  // 状态请求可能交错（定时、切回窗口、操作后各发一次）：只用最后发出的那次的结果，晚回来的旧结果不能盖掉新的
  const reqSeq = useRef(0);
  const failedDevs = useRef(new Set());
  const reload = useCallback(() => {
    const device = selectedDevice();
    const n = ++reqSeq.current;
    // 共享电脑上是别人的客户端：账户、访问方式还是看这台电脑自己的（不显示、不操作对方的账户）
    const shared = device && devRef.current?.byId?.(device)?.shared;
    const get = shared
      ? Promise.all([api.get('/status'), localApi.get('/status')]).then(([s, h]) => ({ ...s, account: h.account, access: h.access, shared_pcs: h.shared_pcs }))
      : api.get('/status');
    return get
      .then((s) => {
        if (n !== reqSeq.current) return;
        failedDevs.current.clear();
        setStatus(s);
        setCloudOperator(s?.account?.session?.user?.nickname || s?.account?.session?.user?.username || '');
        setStatusDevice(device);
        setDown(false);
        rememberStatus(s);
        noteVersion(device, s?.version);
      })
      .catch(() => {
        if (n !== reqSeq.current) return;
        const d = devRef.current;
        if (d?.multi) {
          // 已经有这台电脑的状态：只是一次没取到（网络抖动），留在这台，下一轮再取
          if (statusDevRef.current === device) return;
          // 刚切过去或初始默认这台连不上（含 d.def 本身连不上）：依次退到其他连得上的电脑，不把整个界面锁死在“连不上”
          if (device) failedDevs.current.add(device);
          let links = {};
          try {
            links = window.top.__llrtc?.links?.() ?? {};
          } catch {}
          const cand = d.items
            .filter((x) => x.online && x.id !== device && !failedDevs.current.has(x.id))
            .sort((a, b) => ((links[b.id]?.s === 'online' ? 2 : b.id === d.def ? 1 : 0) - (links[a.id]?.s === 'online' ? 2 : a.id === d.def ? 1 : 0)))[0];
          if (cand) {
            const from = d.byId(device);
            if (from) toastRef.current?.(i18next.t('devices.fallback', { from: from.name, to: cand.name }));
            d.select(cand.id, false);
            return;
          }
        }
        setDown(true);
      });
  }, []);
  const ownDevRef = useRef(null);
  if (!rtcMode && !statusDevice && status?.account?.session?.device_id) {
    ownDevRef.current = status.account.session.device_id;
  }
  // 远程打开（启动页给的列表）和本机 App（本机客户端连其他电脑，服务端 docs/06 §14）都能切换电脑
  const devices = useDeviceState(Boolean(status?.account?.logged_in || status?.shared_pcs), ownDevRef.current);
  if (devices.def) ownDevRef.current = devices.def;
  devRef.current = devices;
  // 设备列表加载后、切换电脑后，取选中那台电脑的状态
  useEffect(() => {
    if (devices.multi || devices.localOnly) reload();
  }, [devices.multi, devices.selected, devices.localOnly, reload]);
  useEffect(() => {
    reload();
    // 远程通道还在连接或连接失败时刷新得勤一点，好尽快看到“已连接”。
    const id = setInterval(reload, relayState === 'connecting' || relayState === 'failed' ? 3000 : 10000);
    const onFocus = () => reload();
    const onOnline = () => {
      failedDevs.current.clear();
      reload();
    };
    window.addEventListener('focus', onFocus);
    window.addEventListener('online', onOnline);
    return () => {
      clearInterval(id);
      window.removeEventListener('focus', onFocus);
      window.removeEventListener('online', onOnline);
    };
  }, [reload, relayState]);

  // 骨架、连不上的页面上也能下拉刷新，也有连接状态（以前要等拿到状态才挂上）
  if (down && !status) {
    return (
      <>
        <Unreachable
          onRetry={() => {
            failedDevs.current.clear();
            reload();
          }}
        />
        <PullRefresh />
        <ConnStatus />
      </>
    );
  }
  if (!status || !devices.ready) {
    return (
      <>
        <main className="shell stack">
          <Skeleton height={60} />
          <Skeleton height={180} style={{ borderRadius: 22 }} />
          <Skeleton height={120} />
        </main>
        <PullRefresh />
        <ConnStatus />
      </>
    );
  }
  let body;
  // 远程打开时一定已经登录；本机、局域网打开不需要登录，没登录时账户、电脑、会员几页显示“开通远程访问”
  if (!status.account.logged_in && status.access !== 'local') {
    body = (
      <PublicLayout>
        <Login />
      </PublicLayout>
    );
  } else if (path === VIEW) {
    body = <ViewPage />;
  } else if (TERM.test(path)) {
    // 按会话 key：换会话时整页重来，不然标签、输入框里的字（草稿）会从上一个会话带过来
    body = <TerminalView key={path.match(TERM)[1]} id={path.match(TERM)[1]} />;
  } else {
    // 换了登录（退出后重新登录）时重置终端页。远程打开时状态来自选中的那台电脑，选中的电脑一变 device_id 就变，
    // 不能拿它当 key：选中的电脑短暂离线时会自动换到默认电脑，终端页重新挂载，打开着的终端小窗就没了。
    // 多台电脑时状态来自选中的那台，换电脑不能换 key；本机 App 用自己的设备 id（列表的 default）。
    const ownId = devices.def ?? ownDevRef.current ?? status.account.session?.device_id;
    const login = rtcMode ? 'remote' : status.account.logged_in && ownId ? `local:${ownId}` : 'guest';
    const home = opensInPage() ? <DraftPane key={`start:${login}`} /> : <SessionHome />;
    const pluginName = parsePluginRoute(path);
    const page = SESS.test(path) ? null : pluginName ? <PluginView name={pluginName} /> : { '/': home, '/new': <DraftPane key={`start:${login}`} />, '/assistant': <Assistant />, '/tasks': <Board />, '/history': <HistoryPage />, '/project': <ProjectPage /> }[path];
    const guest = !status.account.logged_in;
    const drawerPage = route.drawer && { '/settings': <Settings />, '/account': guest ? <Login drawer /> : <Account />, '/devices': <Computers />, '/membership': guest ? <Login drawer /> : <Membership />, '/files': <Files />, '/ssh': <SshHosts />, '/pages': <Pages />, '/plugins': <Plugins />, '/ai': <AiPage />, '/system': <System /> }[route.drawer];
    body = (
      <SessionsProvider key={login} enabled>
        <SessionUiProvider>
          <AssistantSummaryProvider>
            <ConsoleLayout path={path} stale={devices.multi && statusDevice !== devices.selected}>
              {page}
            </ConsoleLayout>
            <PageDrawer path={route.drawer} behind={route.behind} onClose={route.close} stale={devices.multi && statusDevice !== devices.selected}>
              {drawerPage}
            </PageDrawer>
          </AssistantSummaryProvider>
        </SessionUiProvider>
      </SessionsProvider>
    );
  }
  return (
    <StatusCtx.Provider value={{ status, reload }}>
      <DevicesProvider value={devices}>
        <UpdateProvider>
          <ReportProvider>
            <TipsProvider enabled>
              <OnboardTracker />
              {body}
              <PullRefresh />
              <ConnStatus />
            </TipsProvider>
          </ReportProvider>
        </UpdateProvider>
      </DevicesProvider>
    </StatusCtx.Provider>
  );
}

export default function App() {
  return (
    <Router>
      <Shell />
    </Router>
  );
}
