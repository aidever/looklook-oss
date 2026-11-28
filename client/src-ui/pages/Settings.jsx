// 设置：按分区显示，一次只看一个分区（地址里的 #分区 决定）。
//   通用：电脑名称、语言、附件；终端：字体与配色、新终端的默认文件夹和 shell、新 AI 会话默认的权限；
//   网络与访问：远程线路、直连（端口与公网地址）、访问控制；关于：版本、检查更新、问题记录。
// 和 AI 有关的（账号与模型、通知与飞书、工具与权限、宠物）在“AI 助手”页（Ai.jsx），以前指到这里的锚点转过去。
// 终端和通用里的选项改了就自动保存，不需要点保存按钮。
import { ArrowUpRight, Bot, Check, Eraser, Globe, Info, Loader2, Search, SlidersHorizontal, SquareTerminal, X } from 'lucide-react';
import { IdleCleanupDialog } from '../shared/IdleCleanup.jsx';
import { useEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';

import { useStatus } from '../App.jsx';
import { More, Row } from '../shared/page.jsx';
import { LocalOnlyToggle, useDevices } from '../shared/devices.jsx';
import { api } from '../shared/api.js';
import ConnectionCards from './Connection.jsx';
import { AttachmentsCard } from '../shared/Attachments.jsx';
import { BrowserBindingCard } from '../shared/BrowserBinding.jsx';
import { ClaudeLinkGuardCard } from '../shared/linkOpen.jsx';
import { MacFoldersCard } from '../shared/MacFolders.jsx';
import { MOVED_TO_AI } from './Ai.jsx';
import { FolderField, ShellPicker } from './Terminals.jsx';
import { setLocale } from '../shared/i18n.js';
import { TERM_THEMES, termTheme } from '../shared/termTheme.js';
import { DiagCard } from '../shared/Report.jsx';
import RelayPicker, { RELAYS_ANCHOR } from '../shared/RelayPicker.jsx';
import { Link, useRouter } from '../shared/router.jsx';
import { shouldRemind, UpdateCheck } from '../shared/Update.jsx';
import { Button, Card, CopyButton, ErrorNote, Seg, useAction, useLoad, useToast } from '../shared/ui.jsx';
import { useTips } from '../shared/tips.jsx';
import { SyncCard, SyncLabel } from '../shared/SettingsSync.jsx';
import { refreshNativeTerms, useNativeTerms } from '../shared/nativeTerm.js';

// 开源仓库（客户端 AGPL-3.0-or-later）

const SYSTEM_CJK = '"PingFang SC", "Hiragino Sans GB", "Microsoft YaHei UI", "Microsoft YaHei", "Noto Sans Mono CJK SC", "Noto Sans CJK SC", monospace';

export function fontFamily(font) {
  return `"JetBrains Mono", ${font === 'wenkai' ? '"LXGW WenKai Mono", ' : ''}${SYSTEM_CJK}`;
}

/** 在管理台里加载终端字体（用于预览）。 */
function useTerminalFonts(enabled) {
  useEffect(() => {
    if (!enabled || document.getElementById('ll-term-fonts')) return;
    const l = document.createElement('link');
    l.id = 'll-term-fonts';
    l.rel = 'stylesheet';
    l.href = '/fonts/fonts.css';
    document.head.appendChild(l);
  }, [enabled]);
}

// 分段选择挪到了 shared/ui.jsx；别的页面还从这里引用
export { Seg };

/** 终端配色：每个选项是一小块终端的样子（栏、提示符、光标、几种 ANSI 颜色） */
const THEME_IDS = Object.keys(TERM_THEMES);

function ThemePicker({ value, onChange }) {
  const { t } = useTranslation();
  const active = TERM_THEMES[value] ? value : THEME_IDS[0];
  return (
    <div className="theme-pick" role="group" aria-label={t('settings.theme')}>
      {THEME_IDS.map((id) => {
        const th = TERM_THEMES[id];
        const selected = active === id;
        return (
          <button key={id} type="button" aria-pressed={selected} onClick={() => onChange(id)}>
            <span className="theme-sample" style={{ background: th.bg, borderColor: th.border, color: th.fg }}>
              <span className="theme-sample-bar" style={{ background: th.bar, borderColor: th.border }} />
              <span className="theme-sample-line">
                <span style={{ color: th.accent }}>$</span>
                <span>ls</span>
                <i style={{ background: th.accent }} aria-hidden="true" />
              </span>
              <span className="theme-sample-dots" aria-hidden="true">
                {th.ansi.slice(0, 5).map((c) => (
                  <i key={c} style={{ background: c }} />
                ))}
              </span>
            </span>
            <span className="theme-name">
              {selected && <Check size={14} aria-hidden="true" />}
              <span>{t(`settings.theme_${id}`)}</span>
            </span>
          </button>
        );
      })}
    </div>
  );
}

const SECTIONS = [
  ['general', SlidersHorizontal],
  ['terminal', SquareTerminal],
  ['network', Globe],
  ['about', Info],
];
// 别处链接用的锚点（#relays、#access、#public-https）落到所在的分区
const ALIAS = { [RELAYS_ANCHOR]: 'network', access: 'network', 'public-https': 'network' };

/** 跨分区快速搜索索引（含常用同义词和 AI 助手页的快捷入口） */
const SEARCH_ITEMS = [
  { id: 'device_name', section: 'general', anchor: 'set-general', titleKey: 'settings.device_name', hintKey: 'settings.device_name_hint', kw: 'name host computer 电脑 名称 名字 设备名' },
  { id: 'language', section: 'general', anchor: 'set-general', titleKey: 'settings.language', hintKey: 'settings.language_hint', kw: 'lang locale chinese english 中文 英文 语言 界面' },
  { id: 'browser_bind', section: 'general', anchor: 'fingerprint-browser', titleKey: 'browser_bind.card_title', hintKey: 'browser_bind.card_desc', kw: 'fingerprint browser profile 指纹 浏览器 远程' },
  { id: 'claude_guard', section: 'general', anchor: 'claude-link-guard', titleKey: 'link_guard.card_title', hintKey: 'link_guard.card_desc', kw: 'claude link domain ban risk 链接 风控 拦截 防封' },
  { id: 'sync', section: 'general', anchor: 'set-sync', titleKey: 'sync.title', hintKey: 'sync.lead', kw: 'sync devices multi 同步 多设备 云同步' },
  { id: 'idle', section: 'general', anchor: 'set-idle', titleKey: 'idle.card', hintKey: 'idle.auto_hint', kw: 'idle stop cleanup session 后台 会话 清理 休眠 自动停止' },
  { id: 'attachments', section: 'general', anchor: 'set-attachments', titleKey: 'att.title', hintKey: 'att.lead', kw: 'image screenshot file temp cache 附件 截图 图片 清理 保留天数' },
  { id: 'theme', section: 'terminal', anchor: 'set-term-appearance', titleKey: 'settings.theme', hintKey: 'settings.terminal', kw: 'color dark light theme 配色 主题 深色 浅色 终端外观' },
  { id: 'font', section: 'terminal', anchor: 'set-term-appearance', titleKey: 'settings.font', hintKey: 'settings.font_size', kw: 'font size wenkai mono 字体 字号 霞鹜文楷 大小' },
  { id: 'workdir', section: 'terminal', anchor: 'set-term-defaults', titleKey: 'settings.workdir', hintKey: 'settings.workdir_hint', kw: 'folder directory cwd path 默认文件夹 目录 路径' },
  { id: 'shell', section: 'terminal', anchor: 'set-term-defaults', titleKey: 'shell.label', hintKey: 'settings.new_terminal', kw: 'bash zsh fish powershell cmd 终端 shell 命令行' },
  { id: 'full_access', section: 'terminal', anchor: 'set-term-defaults', titleKey: 'settings.default_full_access', hintKey: 'settings.default_full_access_hint', kw: 'permission approve auto ai 权限 审批 确认 全部权限' },
  { id: 'termd', section: 'terminal', anchor: 'set-term-defaults', titleKey: 'settings.termd', hintKey: 'settings.termd_hint', kw: 'tmux ttyd termd engine 引擎 会话引擎' },
  { id: 'connection', section: 'network', anchor: 'public-https', titleKey: 'conn_cfg.title', hintKey: 'conn_cfg.intro', kw: 'lan public ipv6 ipv4 direct https cert port nat tunnel frp cloudflare 直连 局域网 公网 穿透 证书 端口' },
  { id: 'access', section: 'network', anchor: 'access', titleKey: 'conn_cfg.sec_title', hintKey: 'conn_cfg.sec_intro', kw: 'password code whitelist ip security 访问码 密码 白名单 安全' },
  { id: 'relays', section: 'network', anchor: RELAYS_ANCHOR, titleKey: 'relays.title', hintKey: 'relays.intro', kw: 'relay latency speed proxy clash 中转 线路 延迟 测速 代理' },
  { id: 'update', section: 'about', anchor: 'set-about', titleKey: 'settings.check_update', hintKey: 'update.auto_hint', kw: 'version update auto upgrade 版本 更新 自动更新 升级' },
  { id: 'tips', section: 'about', anchor: 'set-about', titleKey: 'tips.reset_label', hintKey: 'tips.reset_hint', kw: 'tips onboarding guide 新手 引导 提示 重置' },
  { id: 'diag', section: 'about', anchor: 'set-diag', titleKey: 'diag.title', hintKey: 'diag.intro', kw: 'log error bug report feedback 日志 报错 反馈 问题 诊断' },
  // 快捷入口：用户常来设置页找 AI 相关配置
  { id: 'ai_accounts', to: '/ai', titleKey: 'settings.sec_ai', hintKey: 'nav.ai', kw: 'claude codex agy model api key account 模型 账号 额度 第三方' },
  { id: 'ai_notify', to: '/ai#notify', titleKey: 'settings.sec_notify', hintKey: 'nav.ai', kw: 'feishu lark bark webhook push notification 通知 飞书 提醒 推送' },
  { id: 'ai_tools', to: '/ai#tools', titleKey: 'guard.title', hintKey: 'nav.ai', kw: 'mcp guard safety command tool 工具 权限 安全网 命令规则' },
  { id: 'ai_pet', to: '/ai#pet', titleKey: 'settings.sec_pet', hintKey: 'nav.ai', kw: 'pet desk mascot 宠物 桌宠' },
];

function sectionOf(hash) {
  const h = hash.replace(/^#/, '');
  const id = ALIAS[h] ?? h;
  return SECTIONS.some(([k]) => k === id) ? id : 'general';
}

// 自动保存的设置项（自动更新、附件保留天数在各自的卡片里单独保存）
const KEYS = ['theme', 'font', 'font_size', 'default_workdir', 'default_shell', 'device_name', 'default_full_access', 'idle_stop_days', 'termd', 'native_terminal'];
const norm = (v, k) => (k === 'termd' ? String(Boolean(v?.[k])) : String(v?.[k] ?? ''));
const differs = (a, b) => (k) => norm(a, k) !== norm(b, k);
const same = (a, b) => !KEYS.some(differs(a, b));

/** 改动停下 600 毫秒后保存；离开页面时把没保存的立即发出去。保存完成后自动淡出提示。 */
function useAutoSave(status, reload) {
  const [s, setS] = useState(status.settings);
  const [state, setState] = useState({ phase: 'idle', error: null });
  const saved = useRef(status.settings);
  const latest = useRef({ s, status });
  const fadeTimer = useRef(null);
  latest.current = { s, status };

  // 只发改了的项：其余的（包括别的电脑刚同步过来的）由后端沿用现在的值
  const body = (v) =>
    Object.fromEntries(
      KEYS.filter(differs(v, saved.current)).map((k) => [
        k,
        k === 'font_size' || k === 'idle_stop_days' ? Number(v[k]) : k === 'termd' ? Boolean(v[k]) : v[k],
      ]),
    );

  // 没有未保存的改动时跟着最新状态走（比如别的电脑同步来了新字号）
  useEffect(() => {
    if (same(latest.current.s, saved.current) && !same(status.settings, saved.current)) {
      saved.current = status.settings;
      setS(status.settings);
    }
  }, [status.settings]);

  useEffect(() => {
    if (same(s, saved.current)) return;
    if (fadeTimer.current) {
      clearTimeout(fadeTimer.current);
      fadeTimer.current = null;
    }
    const id = setTimeout(async () => {
      const sent = s;
      setState({ phase: 'saving', error: null });
      try {
        const r = await api.put('/settings', body(sent));
        saved.current = r;
        // 保存期间又改了的话，不覆盖正在输入的内容
        if (latest.current.s === sent) setS(r);
        setState({ phase: 'saved', error: null });
        fadeTimer.current = setTimeout(() => {
          setState((cur) => (cur.phase === 'saved' ? { phase: 'idle', error: null } : cur));
        }, 2200);
        reload();
      } catch (e) {
        setState({ phase: 'error', error: e });
      }
    }, 600);
    return () => clearTimeout(id);
  }, [s]);

  useEffect(
    () => () => {
      if (fadeTimer.current) clearTimeout(fadeTimer.current);
      if (!same(latest.current.s, saved.current)) api.put('/settings', body(latest.current.s)).then(reload, () => {});
    },
    [],
  );

  return [s, setS, state];
}

/** 后台会话：几天没打开就自动停掉，以及立即清理（shared/IdleCleanup.jsx） */
function IdleCard({ value, onChange }) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  return (
    <Card id="set-idle" title={t('idle.card')}>
      <Row label={<SyncLabel text={t('idle.auto_label')} />} hint={t('idle.auto_hint')} stack>
        <Seg
          value={Number(value)}
          label={t('idle.auto_label')}
          onChange={onChange}
          options={[0, 1, 3, 7, 14, 30].map((d) => [d, d ? t('idle.n_days', { count: d }) : t('idle.never')])}
        />
      </Row>
      <Row label={t('idle.now_label')} hint={t('idle.now_hint')}>
        <Button variant="ghost" icon={Eraser} onClick={() => setOpen(true)}>
          {t('idle.pick')}
        </Button>
      </Row>
      {open && <IdleCleanupDialog onClose={() => setOpen(false)} />}
    </Card>
  );
}

/** 使用提示：看过的一次性提示、上手清单都重新出现（shared/tips.jsx）。 */
function TipsField() {
  const { t } = useTranslation();
  const tips = useTips();
  const toast = useToast();
  const { busy, run } = useAction();
  const reset = () =>
    run(async () => {
      await tips.reset();
      toast(t('tips.reset_done'));
    });
  return (
    <Row label={t('tips.reset_label')} hint={t('tips.reset_hint')}>
      <Button variant="ghost" busy={busy} onClick={reset}>
        {t('tips.reset')}
      </Button>
    </Row>
  );
}

function SavePill({ state }) {
  const { t } = useTranslation();
  if (state.phase !== 'saving' && state.phase !== 'saved') return null;
  return (
    <div className={`save-pill ${state.phase === 'saved' ? 'ok' : ''}`} role="status" aria-live="polite">
      {state.phase === 'saving' ? (
        <>
          <Loader2 size={14} className="spin" aria-hidden="true" />
          <span>{t('settings.saving')}</span>
        </>
      ) : (
        <>
          <Check size={14} aria-hidden="true" />
          <span>{t('settings.saved')}</span>
        </>
      )}
    </div>
  );
}

/** 滚动到指定锚点并短暂高亮，帮用户一眼锁定目标设置项 */
function focusAnchor(anchor) {
  if (!anchor) return;
  setTimeout(() => {
    const el = document.getElementById(anchor);
    if (!el) return;
    el.scrollIntoView({ behavior: 'smooth', block: 'center' });
    el.classList.remove('settings-flash');
    void el.offsetWidth;
    el.classList.add('settings-flash');
    setTimeout(() => el.classList.remove('settings-flash'), 1600);
  }, 60);
}

export default function Settings() {
  const { t, i18n } = useTranslation();
  const { status, reload } = useStatus();
  const { hash, navigate } = useRouter();
  const devices = useDevices();
  const [s, setS, saveState] = useAutoSave(status, reload);
  const [shells] = useLoad(() => api.get('/shells'));
  const [query, setQuery] = useState('');
  const searchRef = useRef(null);
  const tabRefs = useRef([]);
  const caps = status.capabilities;
  const native = useNativeTerms();
  // 本机终端只对这台电脑上的会话有用：管理台在这台电脑上打开、看的是这台电脑的设置时才显示
  const nativeHere = native.items.length > 0 && (!devices.multi || devices.selected === devices.def);
  useTerminalFonts(caps.fonts);
  const set = (k) => (v) => setS({ ...s, [k]: v });
  const section = sectionOf(hash);
  const activeTheme = TERM_THEMES[s.theme] ? s.theme : THEME_IDS[0];
  const th = termTheme(activeTheme);
  const updateDot = shouldRemind(status.update);

  const go = (id) => {
    navigate(`/settings#${id}`, { replace: true });
  };

  // 按 / 快速聚焦搜索框（不在输入框里时）
  useEffect(() => {
    const onKey = (e) => {
      if (e.key !== '/' || e.metaKey || e.ctrlKey || e.altKey) return;
      const tag = document.activeElement?.tagName;
      if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT' || document.activeElement?.isContentEditable) return;
      e.preventDefault();
      searchRef.current?.focus();
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, []);

  const results = useMemo(() => {
    const q = query.trim().toLowerCase();
    if (!q) return [];
    const terms = q.split(/\s+/).filter(Boolean);
    return SEARCH_ITEMS.filter((item) => {
      if (item.id === 'termd' && !caps.termd) return false;
      const hay = [
        t(item.titleKey),
        t(item.hintKey),
        item.section ? t(`settings.sec_${item.section}`) : t('nav.ai'),
        item.kw,
      ]
        .join(' ')
        .toLowerCase();
      return terms.every((term) => hay.includes(term));
    });
  }, [query, t, caps.termd]);

  const jumpTo = (item) => {
    setQuery('');
    if (item.to) {
      navigate(item.to);
      return;
    }
    if (item.section && item.section !== section) go(item.section);
    focusAnchor(item.anchor);
  };

  const onTabKeyDown = (idx) => (e) => {
    if (e.key !== 'ArrowRight' && e.key !== 'ArrowLeft') return;
    e.preventDefault();
    const next = e.key === 'ArrowRight' ? (idx + 1) % SECTIONS.length : (idx - 1 + SECTIONS.length) % SECTIONS.length;
    go(SECTIONS[next][0]);
    tabRefs.current[next]?.focus();
  };

  const versionCopyText = `Looklook ${status.version}${status.target ? ` (${status.target})` : ''}`;

  const sections = {
    general: (
      <>
        <Card id="set-general" title={t('settings.general')}>
          <Row label={t('settings.device_name')} hint={t('settings.device_name_hint')}>
            <div className="srow2-input-wrap">
              <input
                className="srow2-input"
                value={s.device_name}
                onChange={(e) => set('device_name')(e.target.value)}
                maxLength={64}
                placeholder={t('settings.device_name_auto')}
              />
              {s.device_name && (
                <button
                  type="button"
                  className="srow2-clear"
                  aria-label={t('settings.device_name_clear')}
                  title={t('settings.device_name_clear')}
                  onClick={() => set('device_name')('')}
                >
                  <X size={14} aria-hidden="true" />
                </button>
              )}
            </div>
          </Row>
          <Row label={t('settings.language')} hint={devices.multi ? t('settings.language_hint') : undefined}>
            <Seg
              value={i18n.language?.startsWith('en') ? 'en-US' : 'zh-CN'}
              label={t('settings.language')}
              onChange={setLocale}
              options={[
                ['zh-CN', '中文'],
                ['en-US', 'English'],
              ]}
            />
          </Row>
        </Card>
        {caps.os === 'darwin' && status.access === 'local' && <MacFoldersCard />}
        <BrowserBindingCard />
        <ClaudeLinkGuardCard />
        <div id="set-sync" className="anchor">
          <SyncCard />
        </div>
        <IdleCard value={s.idle_stop_days ?? 7} onChange={set('idle_stop_days')} />
        <div id="set-attachments" className="anchor">
          <AttachmentsCard />
        </div>
      </>
    ),
    terminal: (
      <>
        <Card id="set-term-appearance" title={t('settings.terminal')}>
          <div
            className="term-preview"
            style={{
              '--preview-font': fontFamily(s.font),
              '--term-bg': th.bg,
              '--term-bar': th.bar,
              '--term-line': th.border,
              '--term-fg': th.fg,
              '--term-muted': th.muted,
              '--term-accent': th.accent,
            }}
          >
            <div className="term-preview-bar" aria-hidden="true">
              <span className="term-preview-dots">
                <i style={{ background: th.ansi[0] }} />
                <i style={{ background: th.ansi[2] }} />
                <i style={{ background: th.ansi[1] }} />
              </span>
              <span className="term-preview-title">
                {t(`settings.theme_${activeTheme}`)} · {s.font_size}px
              </span>
            </div>
            <div className="font-preview" style={{ color: th.fg, fontSize: `${s.font_size}px` }}>
              {t('settings.preview')}
            </div>
          </div>
          <Row label={<SyncLabel text={t('settings.theme')} />} stack>
            <ThemePicker value={activeTheme} onChange={set('theme')} />
          </Row>
          <Row label={<SyncLabel text={t('settings.font')} />} hint={caps.fonts ? undefined : t('settings.font_missing')}>
            <Seg
              value={s.font}
              label={t('settings.font')}
              onChange={set('font')}
              options={[
                ['wenkai', t('settings.font_wenkai')],
                ['system', t('settings.font_system')],
              ]}
            />
          </Row>
          <Row label={<SyncLabel text={t('settings.font_size')} />}>
            <Seg value={Number(s.font_size)} label={t('settings.font_size')} onChange={set('font_size')} options={[13, 14, 15, 16, 18, 20].map((n) => [n, String(n)])} />
          </Row>
        </Card>
        <Card id="set-term-defaults" title={t('settings.new_terminal')}>
          <FolderField label={t('settings.workdir')} hint={t('settings.workdir_hint')} value={s.default_workdir} onChange={set('default_workdir')} />
          <ShellPicker value={s.default_shell ?? ''} onChange={set('default_shell')} data={shells} />
          <Row label={<SyncLabel text={t('settings.default_full_access')} />} hint={t('settings.default_full_access_hint')}>
            <Seg
              value={Boolean(s.default_full_access)}
              label={t('settings.default_full_access')}
              onChange={set('default_full_access')}
              options={[
                [false, t('settings.default_full_access_ask')],
                [true, t('settings.default_full_access_on')],
              ]}
            />
          </Row>
          {nativeHere && (
            <div className="field">
              <span className="field-label">{t('native.setting')}</span>
              <div className="chips" role="group" aria-label={t('native.setting')}>
                {[{ id: '', name: t('native.builtin') }, ...native.items].map((x) => (
                  <button
                    key={x.id}
                    type="button"
                    className="chip"
                    aria-pressed={(s.native_terminal || '') === x.id}
                    onClick={() => {
                      set('native_terminal')(x.id);
                      setTimeout(refreshNativeTerms, 1500);
                    }}
                  >
                    {x.name}
                  </button>
                ))}
              </div>
              <span className="field-hint">{t('native.setting_hint')}</span>
            </div>
          )}
          {caps.termd && (
            <More summary={t('settings.terminal_advanced')} open={s.termd === false}>
              <Row label={t('settings.termd')} hint={t('settings.termd_hint')}>
                <Seg
                  value={Boolean(s.termd)}
                  label={t('settings.termd')}
                  onChange={set('termd')}
                  options={[
                    [false, t('settings.termd_off')],
                    [true, t('settings.termd_on')],
                  ]}
                />
              </Row>
            </More>
          )}
        </Card>
      </>
    ),
    network: (
      <>
        {(devices.localOnly || (devices.canLocalOnly && (!devices.multi || devices.selected === devices.def))) && (
          <Card title={t('devices.mine')}>
            <LocalOnlyToggle menu={false} />
          </Card>
        )}
        <ConnectionCards />
        <RelayPicker />
      </>
    ),
    about: (
      <>
        <Card id="set-about" title={t('settings.about')}>
          <Row
            label={
              <span className="mono">
                {t('settings.version')} {status.version}
                {/* Windows 版（桌面应用）还在测试阶段 */}
                {status.target?.startsWith('windows') && <> <span className="badge badge-warn">{t('settings.beta')}</span></>}
              </span>
            }
            hint={status.target || undefined}
          >
            <CopyButton text={versionCopyText} variant="ghost" />
          </Row>
          <UpdateCheck />
          {/* Claude Code、Codex、Antigravity 的版本和自动更新在 AI 助手页：在这里找更新的人点一下过去 */}
          <Row label={t('aup.settings_link')} hint={t('aup.settings_link_hint')}>
            <Link to="/ai#updates">{t('aup.settings_link_go')}</Link>
          </Row>
          <TipsField />
          <More summary={t('settings.details')}>
            <dl className="kv">
              <dt>{t('settings.platform')}</dt>
              <dd className="mono">{status.target}</dd>
              <dt>{t('settings.backend')}</dt>
              <dd>{t(caps.persistent ? 'settings.backend_persistent' : 'settings.backend_direct')}</dd>
              <dt>{t('settings.server')}</dt>
              <dd className="mono">{status.account.server}</dd>
              <dt>{t('settings.source')}</dt>
              <dd>AGPL-3.0</dd>
            </dl>
          </More>
        </Card>
        <div id="set-diag" className="anchor">
          <DiagCard />
        </div>
      </>
    ),
  };

  // 以前在这里的 AI 分区（#ai、#notify、#pet、#claude-accounts 等）搬到了 AI 助手页
  const moved = MOVED_TO_AI.has(hash.replace(/^#/, ''));
  useEffect(() => {
    if (moved) navigate(`/ai${hash}`, { replace: true });
  }, [moved, hash, navigate]);

  // 从别处带着 #access 或 #public-https 跳过来时滚到对应卡片
  useEffect(() => {
    const target = hash === '#access' ? 'access' : hash === '#public-https' ? 'public-https' : null;
    if (target) focusAnchor(target);
  }, [hash]);

  return (
    <>
      <div className="settings">
        <div className="settings-head">
          <div className="settings-search">
            <Search size={15} className="settings-search-ico" aria-hidden="true" />
            <input
              ref={searchRef}
              type="search"
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === 'Escape' && query) {
                  e.preventDefault();
                  e.stopPropagation();
                  setQuery('');
                } else if (e.key === 'Enter' && results.length > 0) {
                  e.preventDefault();
                  jumpTo(results[0]);
                }
              }}
              placeholder={t('settings.search_placeholder')}
              aria-label={t('settings.search_placeholder')}
              autoComplete="off"
              spellCheck={false}
            />
            {query ? (
              <button
                type="button"
                className="settings-search-clear"
                aria-label={t('settings.search_clear')}
                onClick={() => {
                  setQuery('');
                  searchRef.current?.focus();
                }}
              >
                <X size={14} aria-hidden="true" />
              </button>
            ) : (
              <kbd className="settings-search-kbd" aria-hidden="true">/</kbd>
            )}
          </div>
          <nav className="settings-nav" aria-label={t('settings.title')}>
            {SECTIONS.map(([id, Icon], idx) => (
              <a
                key={id}
                ref={(el) => {
                  tabRefs.current[idx] = el;
                }}
                href={`/settings#${id}`}
                className={`settings-tab${!query && section === id ? ' active' : ''}`}
                aria-current={!query && section === id ? 'page' : undefined}
                onKeyDown={onTabKeyDown(idx)}
                onClick={(e) => {
                  e.preventDefault();
                  setQuery('');
                  go(id);
                }}
              >
                <Icon size={17} aria-hidden="true" />
                <span>{t(`settings.sec_${id}`)}</span>
                {id === 'about' && updateDot && <span className="dot-badge" aria-label={t('update.badge')} />}
              </a>
            ))}
            {/* AI 账号、通知与飞书、工具、宠物搬到了 AI 助手页：在这里找的人点一下过去 */}
            <a
              href="/ai"
              className="settings-tab settings-tab-ext"
              onClick={(e) => {
                e.preventDefault();
                navigate('/ai');
              }}
            >
              <Bot size={17} aria-hidden="true" />
              <span>{t('nav.ai')}</span>
              <ArrowUpRight size={14} className="settings-tab-arrow" aria-hidden="true" />
            </a>
          </nav>
        </div>
        <div className="settings-body">
          {query.trim() ? (
            <Card title={t('settings.search_results', { count: results.length })}>
              {results.length === 0 ? (
                <p className="muted small" style={{ margin: '4px 0' }}>
                  {t('settings.search_empty', { query: query.trim() })}
                </p>
              ) : (
                <ul className="settings-hits">
                  {results.map((item) => (
                    <li key={item.id}>
                      <button type="button" className="settings-hit" onClick={() => jumpTo(item)}>
                        <span className="settings-hit-main">
                          <span className="settings-hit-title">{t(item.titleKey)}</span>
                          <span className="settings-hit-hint">{t(item.hintKey)}</span>
                        </span>
                        <span className="badge settings-hit-sec">
                          {item.section ? t(`settings.sec_${item.section}`) : t('nav.ai')}
                          {item.to && <ArrowUpRight size={12} aria-hidden="true" />}
                        </span>
                      </button>
                    </li>
                  ))}
                </ul>
              )}
            </Card>
          ) : (
            <>
              {(section === 'general' || section === 'terminal') && <ErrorNote error={saveState.error} />}
              {sections[section]}
            </>
          )}
        </div>
      </div>
      {(section === 'general' || section === 'terminal') && <SavePill state={saveState} />}
    </>
  );
}
