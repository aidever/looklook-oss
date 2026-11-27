// 设置 → Claude Code 账号、Codex 配置：本机默认、（Claude Code）另外登录的官方账号、第三方接口，选一个当默认。
// 新建的会话用默认的（开始页可以另选），之后切换默认不影响已经建好的会话。两个助手共用这里的组件，`agent` 区分。
// 官方账号的登录由 Claude Code 自己完成（后台运行 `claude auth login`，界面显示授权页地址、收授权码），看看不经手令牌。
// 第三方接口可以从“常见服务商”里选（model_presets.rs）：地址填好了，贴上 Key 就行，模型从 models.dev 的列表里选。
import { Check, CircleCheck, CircleX, Cpu, ExternalLink, Gauge, KeyRound, ListRestart, LogIn, Plus, Server, SlidersHorizontal, Sparkles, UserRound, Zap } from 'lucide-react';
import { createContext, useContext, useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';

import { api as baseApi } from '../shared/api.js';
import { errorText } from '../shared/i18n.js';
import PopMenu from '../shared/PopMenu.jsx';
import { useRouter } from '../shared/router.jsx';
import { formatTime } from '../shared/time.js';
import { Button, Card, CopyButton, ErrorNote, Field, FoldNote, Modal, Note, PasswordInput, Skeleton, Spinner, useAction, useConfirm, useLoad, useToast } from '../shared/ui.jsx';
import { Disclaimer, ProviderLogo } from './ModelProviders.jsx';
import Select from '../shared/Select.jsx';

// 账号是哪台电脑上的：设置页里是顶栏选中的那台；对话页里的“账号”弹窗（/login）是会话所在那台（deviceApi）
const ApiCtx = createContext(baseApi);
const useApi = () => useContext(ApiCtx);
export const ProfilesApi = ApiCtx.Provider;

const CUSTOM = 'custom';
// 本机的 Ollama（docs/PLAN_2026-10-27.md §4）：检测到才出现，一键填好地址和 Key，模型从已下载的里选
const OLLAMA = 'ollama';
const MODEL_KEYS = ['main', 'fable', 'opus', 'sonnet', 'haiku'];
// 每项对应的 Claude Code 环境变量（标签下面小字显示，名字长，单独一行可折行）
const MODEL_ENV = {
  main: 'ANTHROPIC_MODEL',
  fable: 'ANTHROPIC_DEFAULT_FABLE_MODEL',
  opus: 'ANTHROPIC_DEFAULT_OPUS_MODEL',
  sonnet: 'ANTHROPIC_DEFAULT_SONNET_MODEL',
  haiku: 'ANTHROPIC_DEFAULT_HAIKU_MODEL',
};

/** 服务商列出的模型里给 Claude 的一个家族挑一个：名字里带家族名的，版本号最大的那个 */
function guessModel(ids, family) {
  const hits = ids.filter((id) => id.toLowerCase().includes(family));
  return hits.sort((a, b) => b.localeCompare(a, undefined, { numeric: true }))[0] ?? '';
}

/** 测试结果：每个档位一行，成功的写用时，失败的写服务商的原话 */
function TestResults({ agent, data, catalog }) {
  const { t } = useTranslation();
  if (data.error) return <Note kind="error">{t(data.error === 'claude_missing' ? 'cprof.test_no_claude' : 'cprof.test_failed', { error: data.error })}</Note>;
  const slotName = (slot) => {
    if (slot === 'main') return t('cprof.test_main');
    if (agent === 'codex') return catalog?.find((m) => m.value === slot)?.label ?? slot;
    return t(`cprof.model.${slot}`);
  };
  return (
    <ul className="cprof-test">
      {data.results.map((r) => (
        <li key={r.slot} className={r.ok ? 'ok' : 'bad'}>
          {r.ok ? <CircleCheck size={16} aria-label={t('cprof.test_ok')} /> : <CircleX size={16} aria-label={t('cprof.test_bad')} />}
          <span className="cprof-test-body">
            <b>{slotName(r.slot)}</b> <span className="mono small muted">{r.model ?? r.actual ?? t('cprof.test_unmapped')}</span>
            {r.ok ? <span className="small muted"> · {(r.ms / 1000).toFixed(1)}s</span> : <span className="cprof-test-err small">{r.error === 'timeout' ? t('cprof.test_timeout') : r.error}</span>}
          </span>
        </li>
      ))}
    </ul>
  );
}

// 每个助手的接口路径和文案前缀
const AGENTS = {
  claude: { base: '/claude-profiles', t: 'cprof', anchor: 'claude-accounts' },
  codex: { base: '/codex-profiles', t: 'xprof', anchor: 'codex-profiles' },
  agy: { base: '/agy-profiles', t: 'aprof', anchor: 'agy-accounts' },
};
const PRESET = 'preset:';

/** 配置显示的名字：本机默认账号没有名字；另外的官方账号登录成功后才有（登录的邮箱） */
export function profileName(t, p, agent = 'claude') {
  return p.id === 'official' ? p.name || t(`${AGENTS[agent].t}.builtin`) : p.name || p.email || t(agent === 'agy' ? 'aprof.unnamed' : 'cprof.unnamed');
}

/** models.dev 的价格（美元 / 百万 token）：“$1.2 / $4.2” */
function priceText(cost) {
  if (!cost) return '';
  const f = (n) => `$${Number(n.toFixed(n < 1 ? 3 : 2))}`;
  return `${f(cost.input ?? 0)} / ${f(cost.output ?? 0)}`;
}

/** 模型名输入框，下拉里是这家现在的模型（models.dev，带价格），也能自己填 */
function ModelInput({ id, value, onChange, available, placeholder }) {
  const { t } = useTranslation();
  const hit = available?.find((m) => m.id === value);
  return (
    <>
      <input className="mono" list={available?.length ? id : undefined} value={value} onChange={(e) => onChange(e.target.value)} maxLength={100} spellCheck={false} autoCapitalize="none" placeholder={placeholder} />
      {available?.length > 0 && (
        <datalist id={id}>
          {available.map((m) => (
            <option key={m.id} value={m.id}>
              {[m.name, priceText(m.cost)].filter(Boolean).join(' · ')}
            </option>
          ))}
        </datalist>
      )}
      {hit?.cost && <span className="field-hint">{t('cprof.price', { price: priceText(hit.cost) })}</span>}
    </>
  );
}

export function KindIcon({ p, size = 15 }) {
  const Icon = p.kind === 'api' ? Server : UserRound;
  return <Icon size={size} aria-hidden="true" />;
}

/** 官方账号的登录状态：每个单独问（`claude auth status` 要一两秒）。`expired`：查用量时发现登录失效了 */
function LoginStatus({ s, err, expired, agent = 'claude' }) {
  const { t } = useTranslation();
  if (err || s?.error) return <span className="muted">{t(s?.error === 'not_installed' ? (agent === 'agy' ? 'aprof.not_installed' : 'cprof.not_installed') : 'cprof.status_failed')}</span>;
  if (!s) return <span className="muted">{t('cprof.checking')}</span>;
  if (!s.logged_in) return <span className="cprof-warn">{t(expired ? 'cprof.expired' : 'cprof.logged_out')}</span>;
  // 本机默认账号也可能是环境变量里的 Key（authMethod 不是 claude.ai）
  const who = s.email || (s.auth_method && s.auth_method !== 'claude.ai' ? t(`cprof.auth.${s.auth_method}`, { defaultValue: s.auth_method }) : '');
  return (
    <span>
      <Check size={14} style={{ verticalAlign: '-2px', color: 'var(--ok)' }} /> {who || t('cprof.logged_in')}
      {s.plan && <span className="muted"> · {String(s.plan).replace(/^\w/, (c) => c.toUpperCase())}</span>}
    </span>
  );
}

function compactTokens(n) {
  if (n >= 1e9) return `${(n / 1e9).toFixed(2)}B`;
  if (n >= 1e6) return `${(n / 1e6).toFixed(1)}M`;
  if (n >= 1e3) return `${(n / 1e3).toFixed(1)}K`;
  return String(n ?? 0);
}

/** 查到的订阅用量：“5 小时 5% · 7 天 91%”，用得多的标黄，重置时间放在提示里；agy 另支持模型分组额度与本地今日/7天统计 */
function UsageLine({ limits }) {
  const { t } = useTranslation();
  const wins = [
    ['usage_5h', limits?.five_hour],
    ['usage_7d', limits?.seven_day],
  ].filter(([, w]) => w);
  const groups = (limits?.groups ?? []).filter((g) => g && typeof g.used === 'number');
  const localToday = limits?.local_stats?.today;
  const local7d = limits?.local_stats?.seven_day;
  const hasToday = Boolean(localToday && (localToday.requests > 0 || localToday.tokens > 0));
  const has7d = Boolean(local7d && (local7d.requests > 0 || local7d.tokens > 0));
  if (!wins.length && !groups.length && !hasToday && !has7d) return <span className="muted">{t('cprof.usage_empty')}</span>;
  let sep = false;
  const dot = () => {
    const s = sep ? ' · ' : '';
    sep = true;
    return s;
  };
  return (
    <>
      {wins.map(([key, w]) => (
        <span key={key} className={w.used > 85 ? 'cprof-warn' : 'muted'} title={w.resets_at ? t('cprof.usage_reset', { at: formatTime(w.resets_at) }) : undefined}>
          {dot()}
          {t(`cprof.${key}`, { used: Math.round(w.used) })}
          {/* 用完了：直接写出什么时候重置；服务商没给时间就照实说不知道 */}
          {w.used >= 100 && ` (${w.resets_at ? t('cprof.usage_reset', { at: formatTime(w.resets_at) }) : t('cprof.usage_reset_unknown')})`}
        </span>
      ))}
      {groups.map((g, i) => (
        <span key={g.id ?? g.label} className={g.used > 85 ? 'cprof-warn' : 'muted'} title={g.resets_at ? t('cprof.usage_reset', { at: formatTime(g.resets_at) }) : undefined}>
          {dot()}
          {/* agy：每组（Gemini / Claude 和 GPT）各有每周、5 小时两格，组名只在第一格前写一次 */}
          {g.window
            ? `${groups[i - 1]?.group === g.group ? '' : `${g.group} `}${t(`cprof.usage_win_${g.window}`, { used: Math.round(g.used), defaultValue: `${g.window} ${Math.round(g.used)}%` })}`
            : `${g.label || g.id} ${Math.round(g.used)}%`}
          {g.used >= 100 && ` (${g.resets_at ? t('cprof.usage_reset', { at: formatTime(g.resets_at) }) : t('cprof.usage_reset_unknown')})`}
        </span>
      ))}
      {hasToday && (
        <span className="muted">
          {dot()}
          {t('cprof.usage_today', { tokens: compactTokens(localToday.tokens), requests: localToday.requests })}
        </span>
      )}
      {has7d && (
        <span className="muted">
          {dot()}
          {t('cprof.usage_7d_local', { tokens: compactTokens(local7d.tokens) })}
        </span>
      )}
      {/* 开了额外用量（额度用完后按量付费）：这个月花了多少 */}
      {limits.extra?.enabled && (
        <span className="muted">
          {' · '}
          {t('cprof.usage_extra', { used: `${limits.extra.currency === 'USD' ? '$' : ''}${(limits.extra.used ?? 0).toFixed(2)}${limits.extra.currency === 'USD' ? '' : ` ${limits.extra.currency}`}` })}
        </span>
      )}
      {/* 第三方接口的用量是问服务商的（不是正式接口），标明出处 */}
      {limits.provider && <span className="muted"> · {t('cprof.usage_from', { provider: limits.provider })}</span>}
    </>
  );
}

/** 登录失败时 Claude Code / agy 说的话：授权码被拒（400）换成能照着做的说明 */
function loginError(t, st) {
  if (!st.error) return t('cprof.login_failed');
  if (/status code 400\b/.test(st.error)) return t('cprof.login_code_rejected');
  return t('cprof.login_failed_detail', { detail: st.error });
}

/**
 * 登录官方账号：Claude Code 在后台运行 `claude auth login`，Antigravity 在后台运行 `agy acp --login`。
 * 这里只显示授权页地址（旁边的按钮打开、复制），用户在浏览器登录后把授权码或回调链接贴回来。
 * `onClose(ok)`：登录成功时为 true。
 */
function LoginModal({ agent = 'claude', profile, onClose }) {
  const api = useApi();
  const { t } = useTranslation();
  const toast = useToast();
  const cfg = AGENTS[agent];
  const isAgy = agent === 'agy';
  const [st, setSt] = useState(null);
  const [code, setCode] = useState('');
  const { busy, error, setError, run } = useAction();
  const start = () => {
    setSt(null);
    setCode('');
    run(async () => setSt(await api.post(`${cfg.base}/${profile.id}/login`, {})));
  };
  useEffect(start, [agent, profile.id]);
  // 等授权页地址、等换令牌的结果（工具自己打开了本机浏览器时，不用贴授权码也会完成）
  useEffect(() => {
    if (st?.state !== 'running') return undefined;
    const timer = setTimeout(async () => {
      const r = await api.get(`${cfg.base}/${profile.id}/login`).catch(() => null);
      if (r) setSt(r);
    }, 1500);
    return () => clearTimeout(timer);
  }, [st]);
  useEffect(() => {
    if (st?.state !== 'ok') return;
    toast(t('cprof.login_done', { name: profileName(t, profile, agent) }));
    onClose(true);
  }, [st?.state]);
  const close = () => {
    if (st?.state === 'running') api.del(`${cfg.base}/${profile.id}/login`).catch(() => {});
    onClose(false);
  };
  const submit = (e) => {
    e.preventDefault();
    setError(null);
    run(async () => setSt(await api.post(`${cfg.base}/${profile.id}/login/code`, { code: code.trim() })));
  };
  const waiting = st?.state === 'running' && st.code_sent;
  return (
    <Modal open onClose={close} title={profile.fresh ? t(isAgy ? 'aprof.add_official' : 'cprof.add_official') : t('cprof.login_title', { name: profileName(t, profile, agent) })}>
      {profile.fresh && <p className="small muted">{t(isAgy ? 'aprof.official_intro' : 'cprof.official_intro')}</p>}
      {!st && !error && <Skeleton height={80} />}
      {st?.state === 'running' && !st.url && (
        <p className="muted small">
          <Spinner size={14} /> {t(isAgy ? 'aprof.login_starting' : 'cprof.login_starting')}
        </p>
      )}
      {st?.url && st.state !== 'ok' && (
        <form onSubmit={submit}>
          <Field label={t('cprof.login_url')} hint={t(isAgy ? 'aprof.login_url_hint' : 'cprof.login_url_hint')}>
            <div className="cprof-url">
              <code className="mono">{st.url}</code>
              <span className="inline">
                <Button variant="secondary" icon={ExternalLink} onClick={() => window.open(st.url, '_blank', 'noopener')}>
                  {t('action.open')}
                </Button>
                <CopyButton text={st.url} variant="secondary" />
              </span>
            </div>
          </Field>
          <Field label={isAgy ? t('aprof.login_code_label') : 'Authentication Code'} hint={t(isAgy ? 'aprof.login_code_hint' : 'cprof.login_code_hint')}>
            <input className="mono" value={code} onChange={(e) => setCode(e.target.value)} autoComplete="off" spellCheck={false} autoCapitalize="none" disabled={st.state !== 'running' || waiting} autoFocus />
          </Field>
          {st.state === 'failed' && <Note kind="warn">{loginError(t, st)}</Note>}
          <ErrorNote error={error} />
          <div className="modal-actions">
            <Button variant="ghost" onClick={close}>
              {t('action.cancel')}
            </Button>
            {st.state === 'failed' ? (
              <Button onClick={start} busy={busy}>
                {t('cprof.login_retry')}
              </Button>
            ) : (
              <Button type="submit" busy={busy || waiting} disabled={!code.trim()}>
                {t('cprof.login_submit')}
              </Button>
            )}
          </div>
        </form>
      )}
      {(!st?.url || st.state === 'ok') && (
        <>
          {st?.state === 'failed' && <Note kind="warn">{loginError(t, st)}</Note>}
          <ErrorNote error={error} />
          <div className="modal-actions">
            <Button variant="ghost" onClick={close}>
              {t('action.cancel')}
            </Button>
            {(st?.state === 'failed' || error) && (
              <Button onClick={start} busy={busy}>
                {t('cprof.login_retry')}
              </Button>
            )}
          </div>
        </>
      )}
    </Modal>
  );
}

/** 改官方账号的名字（添加时不用起名字，登录成功后用邮箱） */
function OfficialForm({ agent = 'claude', initial, onClose, onSaved }) {
  const api = useApi();
  const { t } = useTranslation();
  const cfg = AGENTS[agent];
  const [name, setName] = useState(initial.name);
  // 本机默认账号：名字可以清空，回到“本机默认账号”
  const builtin = initial.id === 'official';
  const { busy, error, run } = useAction();
  const save = (e) => {
    e.preventDefault();
    run(async () => onSaved(await api.put(`${cfg.base}/${initial.id}`, { name })));
  };
  return (
    <Modal open onClose={onClose} title={t('cprof.rename')}>
      <form onSubmit={save}>
        <Field label={t('cprof.name')}>
          <input value={name} onChange={(e) => setName(e.target.value)} maxLength={40} placeholder={builtin ? t(`${cfg.t}.builtin`) : t(agent === 'agy' ? 'aprof.name_ph_official' : 'cprof.name_ph_official')} required={!builtin} autoFocus />
        </Field>
        <ErrorNote error={error} />
        <div className="modal-actions">
          <Button variant="ghost" onClick={onClose}>
            {t('action.cancel')}
          </Button>
          <Button type="submit" busy={busy} disabled={!builtin && !name.trim()}>
            {t('action.save')}
          </Button>
        </div>
      </form>
    </Modal>
  );
}

/**
 * 第三方接口：平台推荐的服务商（含推广）、常见服务商的快速配置（贴上 Key 就行）或自己填地址；Key 只存在这台电脑上。
 * Claude Code 还能一键用本机的 Ollama。
 */
function ApiForm({ agent, initial, accepted, onClose, onSaved }) {
  const api = useApi();
  const { t, i18n } = useTranslation();
  const cfg = AGENTS[agent];
  const client = agent === 'codex' ? 'codex' : 'claude_code';
  const [data] = useLoad(() => api.get(`/model-providers?locale=${i18n.language}`).catch(() => ({ providers: [], error: 'x' })), [i18n.language]);
  const [presetData] = useLoad(() => api.get(`/model-presets?agent=${agent}&locale=${i18n.language}`).catch(() => ({ presets: [] })), [agent, i18n.language]);
  const providers = (data?.providers ?? []).filter((p) => p.clients.some((c) => c.client === client));
  const presets = presetData?.presets ?? [];
  const [pick, setPick] = useState(initial ? (initial.preset ? PRESET + initial.preset : (initial.provider_id ?? CUSTOM)) : null);
  const [name, setName] = useState(initial?.name ?? '');
  const [baseUrl, setBaseUrl] = useState(initial && !initial.provider_id && !initial.preset ? initial.base_url : '');
  // Claude Code：四个档位；用快速配置时只问主模型和快速模型（Opus、Sonnet 档跟主模型）
  const [models, setModels] = useState(() => Object.fromEntries(MODEL_KEYS.map((k) => [k, initial?.models?.[k] ?? ''])));
  const [main, setMain] = useState(agent === 'codex' ? (initial?.model ?? '') : (initial?.models?.main ?? ''));
  const [fast, setFast] = useState(initial?.models?.haiku ?? '');
  const [key, setKey] = useState('');
  // Codex：模型对应（Codex 的模型名 → 服务商的）
  const [codexMap, setCodexMap] = useState(() => ({ ...(initial?.models ?? {}) }));
  // 服务商自己列出的模型（点“获取”后才有）；测试结果
  const [remote, setRemote] = useState(null);
  const [testRes, setTestRes] = useState(null);
  const fetchAct = useAction();
  const testAct = useAction();
  const [catalog] = useLoad(() => (agent === 'codex' ? api.get('/codex-profiles/catalog').then((r) => r.models, () => []) : Promise.resolve(null)), [agent]);
  // Key 放 x-api-key 头（ANTHROPIC_API_KEY）：Anthropic Console 的 Key（sk-ant-api…）只认这个，填进来时自动勾上
  const [xApiKey, setXApiKey] = useState(initial?.auth === 'api_key');
  const [agree, setAgree] = useState(false);
  const { busy, error, run } = useAction();
  const [local] = useLoad(() => (initial || agent !== 'claude' ? Promise.resolve(null) : api.get('/ollama').then((r) => r.ollama, () => null)), []);
  const [localModel, setLocalModel] = useState('');
  const ollama = pick === OLLAMA && local;
  // 列表到了还没选：有推荐的选第一个，没有就选第一个常见服务商
  useEffect(() => {
    if (pick === null && data && presetData) setPick(providers[0]?.id ?? (presets[0] ? PRESET + presets[0].id : CUSTOM));
  }, [data, presetData]);
  const preset = pick?.startsWith(PRESET) ? presets.find((p) => PRESET + p.id === pick) : null;
  // 换了一家常见服务商：模型换成这家推荐的（编辑原来那家时保留已填的）
  useEffect(() => {
    if (!preset || (initial && PRESET + initial.preset === pick)) return;
    setMain(preset.models.main ?? '');
    setFast(preset.models.fast ?? '');
  }, [pick, Boolean(preset)]);
  const chosen = providers.find((p) => p.id === pick);
  const endpoint = chosen?.clients.find((c) => c.client === client)?.base_url;
  const keepKey = Boolean(initial?.key_set);
  const urlChanged = !initial || pick !== (initial.preset ? PRESET + initial.preset : (initial.provider_id ?? CUSTOM)) || (pick === CUSTOM && baseUrl.trim() !== initial.base_url);
  const ready = ollama
    ? (accepted || agree) && local.new_enough && Boolean(localModel || local.models[0])
    : pick !== null && pick !== OLLAMA && (accepted || agree) && (pick !== CUSTOM || (name.trim() && baseUrl.trim())) && (keepKey || key.trim());
  const save = (e) => {
    e.preventDefault();
    run(async () => {
      if (ollama) {
        // Ollama 不认 Key（随便填），所有档位都用选的模型：Claude Code 的后台小任务默认用 haiku，本机没有
        const m = localModel || local.models[0];
        const r = await api.post(cfg.base, {
          kind: 'api',
          name: name.trim() || 'Ollama',
          base_url: local.base_url,
          api_key: 'ollama',
          models: Object.fromEntries(MODEL_KEYS.map((k) => [k, m])),
          accept: !accepted && agree,
        });
        onSaved(r);
        return;
      }
      const body = {
        kind: 'api',
        name: pick === CUSTOM || initial || name.trim() ? name : undefined,
        api_key: key.trim() || undefined,
        accept: !accepted && agree,
        ...modelBody(),
      };
      if (urlChanged) Object.assign(body, source());
      const r = initial ? await api.put(`${cfg.base}/${initial.id}`, body) : await api.post(cfg.base, body);
      onSaved(r);
    });
  };
  // 地址从哪来（自己填的、快速配置、平台推荐的）
  const source = () => (pick === CUSTOM ? { base_url: baseUrl } : preset ? { preset: preset.id } : { provider_id: pick });
  // 模型和 Key 的发法
  const modelBody = () => {
    if (agent === 'codex') return { model: main.trim(), models: Object.fromEntries(Object.entries(codexMap).map(([k, v]) => [k, v.trim()]).filter(([, v]) => v)) };
    if (preset) return { models: { main: main.trim(), fable: main.trim(), opus: main.trim(), sonnet: main.trim(), haiku: fast.trim() }, auth: preset.auth };
    return { models, auth: xApiKey ? 'api_key' : 'bearer' };
  };
  // 测试、获取模型列表：用表单里现在填的（还没保存也行），Key 没填时用保存的
  const draft = () => (ollama ? { base_url: local.base_url, api_key: 'ollama', models: Object.fromEntries(MODEL_KEYS.map((k) => [k, localModel || local.models[0]])) } : { id: initial?.id, api_key: key.trim() || undefined, ...modelBody(), ...source() });
  const canProbe = ollama ? Boolean(local.models[0]) : pick !== null && pick !== OLLAMA && (pick !== CUSTOM || baseUrl.trim()) && (keepKey || key.trim());
  const runTest = () => {
    setTestRes(null);
    testAct.run(async () => setTestRes(await api.post(`${cfg.base}/test`, draft())));
  };
  // 拿服务商的模型列表：给每一档下拉选；Claude 还没填的档位按名字猜一个（含 fable/opus/… 的）
  const fetchModels = () =>
    fetchAct.run(async () => {
      const r = await api.post(`${cfg.base}/provider-models`, draft());
      if (r.error || !r.models?.length) {
        setRemote([]);
        return;
      }
      setRemote(r.models);
      if (agent === 'claude' && !preset) {
        setModels((m) => Object.fromEntries(MODEL_KEYS.map((k) => [k, m[k] || (k === 'main' ? '' : guessModel(r.models, k))])));
      }
    });
  // 下拉里的：快速配置自带的（带价格）+ 服务商列出的
  const choices = [...(preset?.available ?? []), ...(remote ?? []).filter((id) => !preset?.available?.some((m) => m.id === id)).map((id) => ({ id }))];
  // “从服务商获取模型”按钮和结果说明
  const fetchRow = (
    <div className="cprof-fetch">
      <Button variant="ghost" icon={ListRestart} busy={fetchAct.busy} disabled={!canProbe} onClick={fetchModels}>
        {t('cprof.fetch_models')}
      </Button>
      {remote && <span className="small muted">{remote.length ? t('cprof.fetch_count', { n: remote.length }) : t('cprof.fetch_none')}</span>}
      <ErrorNote error={fetchAct.error} />
    </div>
  );
  const option = (id, head, desc, extra) => (
    <button key={id} type="button" role="radio" aria-checked={pick === id} className="launch model-option" onClick={() => setPick(id)}>
      <span className="model-option-head">
        {head}
        {extra}
      </span>
      {desc && <span className="small muted">{desc}</span>}
    </button>
  );
  return (
    <Modal open onClose={onClose} title={t(initial ? `${cfg.t}.edit_api` : `${cfg.t}.add_api`)} wide>
      <form onSubmit={save}>
        {accepted ? (
          <FoldNote kind="warn" summary={t('model.disclaimer.title')}>
            <Disclaimer />
          </FoldNote>
        ) : (
          <Note kind="warn">
            <b>{t('model.disclaimer.title')}</b>
            <Disclaimer />
            <label className="check-row" style={{ margin: 0 }}>
              <input type="checkbox" checked={agree} onChange={(e) => setAgree(e.target.checked)} />
              <span>{t('model.disclaimer.agree')}</span>
            </label>
          </Note>
        )}
        <p className="small muted">{t(`${cfg.t}.api_intro`)}</p>
        {(!data || !presetData) && <Skeleton height={64} />}
        {data?.error && <Note kind="warn">{t('model.list_failed')}</Note>}
        {data && presetData && (
          <div role="radiogroup" aria-label={t('model.provider')}>
            {providers.length > 0 && (
              <div className="model-grid">
                {providers.map((p) =>
                  option(
                    p.id,
                    <>
                      <ProviderLogo p={p} />
                      <b className="grow ellipsis">{p.name}</b>
                    </>,
                    p.tagline,
                    <span className="badge">{t('promotion.label')}</span>,
                  ),
                )}
              </div>
            )}
            {presets.length > 0 && <p className="small muted model-group">{t('cprof.preset_section')}</p>}
            <div className="model-grid model-grid-compact">
              {presets.map((p) => option(PRESET + p.id, <b className="grow ellipsis">{p.name}</b>))}
              {local &&
                option(
                  OLLAMA,
                  <>
                    <Cpu size={15} aria-hidden="true" />
                    <b className="grow">{t('cprof.ollama')}</b>
                  </>,
                )}
              {option(
                CUSTOM,
                <>
                  <Server size={15} aria-hidden="true" />
                  <b className="grow">{t('model.custom')}</b>
                </>,
              )}
            </div>
          </div>
        )}
        {chosen && (
          <div className="model-detail">
            {chosen.description && <p className="small" style={{ whiteSpace: 'pre-wrap' }}>{chosen.description}</p>}
            <p className="small muted" style={{ margin: 0 }}>
              {t('model.endpoint')} <span className="mono">{endpoint}</span> ·{' '}
              <a href={chosen.homepage_url} target="_blank" rel="noopener noreferrer sponsored">
                {t('model.homepage')}
              </a>
            </p>
          </div>
        )}
        {preset && (
          <div className="model-detail">
            <p className="small muted" style={{ margin: 0 }}>
              {t('model.endpoint')} <span className="mono">{preset.base_url}</span> ·{' '}
              <a href={preset.key_url} target="_blank" rel="noopener noreferrer">
                {t('cprof.preset_key')} <ExternalLink size={13} style={{ verticalAlign: '-2px' }} />
              </a>
            </p>
          </div>
        )}
        {pick === CUSTOM && <p className="small muted">{t('model.custom_desc')}</p>}
        {ollama && (
          <div className="model-detail">
            <p className="small" style={{ margin: 0 }}>
              {t('cprof.ollama_intro')} <span className="mono">{local.base_url}</span>
            </p>
            {!local.new_enough && <Note kind="warn">{t('cprof.ollama_old', { version: local.version })}</Note>}
            {local.models.length === 0 && <Note kind="warn">{t('cprof.ollama_no_models')}</Note>}
          </div>
        )}
        {ollama && local.models.length > 0 && (
          <Field label={t('cprof.ollama_model')} hint={t('cprof.ollama_model_hint')}>
            <Select value={localModel || local.models[0]} onChange={(e) => setLocalModel(e.target.value)}>
              {local.models.map((m) => (
                <option key={m} value={m}>
                  {m}
                </option>
              ))}
            </Select>
          </Field>
        )}
        {!ollama && (
          <Field label="API Key" hint={keepKey ? t('model.key_keep') : t('model.key_hint')}>
            <PasswordInput
              value={key}
              onChange={(e) => {
                setKey(e.target.value);
                if (e.target.value.trim().startsWith('sk-ant-api')) setXApiKey(true);
              }}
              autoComplete="off"
              spellCheck={false}
              placeholder={keepKey ? '••••••••' : 'sk-…'}
              autoFocus={Boolean(preset) && !initial}
            />
          </Field>
        )}
        {!ollama && (preset || agent === 'codex') && (
          <div className="cprof-models">
            <Field label={t(agent === 'codex' ? 'cprof.model_codex' : 'cprof.model_main')} hint={agent === 'codex' ? t('model.model_hint') : undefined}>
              <ModelInput id="ll-model-main" value={main} onChange={setMain} available={choices} placeholder={t('cprof.model_default')} />
            </Field>
            {agent === 'claude' && (
              <Field label={t('cprof.model_fast')} hint={t('cprof.model_fast_hint')}>
                <ModelInput id="ll-model-fast" value={fast} onChange={setFast} available={choices} placeholder={t('cprof.model_default')} />
              </Field>
            )}
          </div>
        )}
        {(pick === CUSTOM || chosen || (preset && !initial) || initial) && !ollama && (
          <Field label={t('cprof.name')} hint={pick !== CUSTOM && !initial ? t('cprof.name_hint_provider') : undefined}>
            <input value={name} onChange={(e) => setName(e.target.value)} maxLength={40} placeholder={preset?.name ?? chosen?.name ?? ''} required={pick === CUSTOM} />
          </Field>
        )}
        {pick === CUSTOM && (
          <Field label={t('model.custom_url')} hint={t(`model.custom_url_hint_${agent}`)}>
            <input value={baseUrl} onChange={(e) => setBaseUrl(e.target.value)} placeholder="https://api.example.com" inputMode="url" spellCheck={false} autoCapitalize="none" required />
          </Field>
        )}
        {agent === 'claude' && !ollama && !preset && (
          <>
            <label className="check-row">
              <input type="checkbox" checked={xApiKey} onChange={(e) => setXApiKey(e.target.checked)} />
              <span>
                {t('cprof.auth_api_key')}
                <span className="small muted" style={{ display: 'block' }}>
                  {t('cprof.auth_api_key_hint')}
                </span>
              </span>
            </label>
            <FoldNote summary={t('cprof.models')} open={Boolean(remote) || MODEL_KEYS.some((k) => initial?.models?.[k])}>
              <p className="small muted" style={{ marginTop: 0 }}>
                {t('cprof.models_hint')}
              </p>
              {fetchRow}
              <div className="cprof-models">
                {MODEL_KEYS.map((k) => (
                  <Field
                    key={k}
                    label={
                      <>
                        {t(`cprof.model.${k}`)}
                        <span className="cprof-env mono">{MODEL_ENV[k]}</span>
                      </>
                    }
                  >
                    <ModelInput id={`ll-model-${k}`} value={models[k]} onChange={(v) => setModels((m) => ({ ...m, [k]: v }))} available={choices} />
                  </Field>
                ))}
              </div>
            </FoldNote>
          </>
        )}
        {agent === 'codex' && !ollama && (
          <FoldNote summary={t('xprof.map')} open={Boolean(remote) || Object.keys(initial?.models ?? {}).length > 0}>
            <p className="small muted" style={{ marginTop: 0 }}>
              {t('xprof.map_hint')}
            </p>
            {fetchRow}
            {!catalog && <Skeleton height={40} />}
            {catalog?.length === 0 && <p className="small muted">{t('xprof.map_no_catalog')}</p>}
            <div className="cprof-models">
              {[...(catalog ?? []).filter((m) => m.section !== 'more' || codexMap[m.value]), ...Object.keys(codexMap).filter((k) => !catalog?.some((m) => m.value === k)).map((k) => ({ value: k, label: k }))].map((m) => (
                <Field
                  key={m.value}
                  label={
                    <>
                      {m.label}
                      {m.label !== m.value && <span className="cprof-env mono">{m.value}</span>}
                    </>
                  }
                >
                  <ModelInput id={`ll-xmap-${m.value}`} value={codexMap[m.value] ?? ''} onChange={(v) => setCodexMap((c) => ({ ...c, [m.value]: v }))} available={choices} placeholder={t('xprof.map_ph')} />
                </Field>
              ))}
            </div>
          </FoldNote>
        )}
        <div className="cprof-test-row">
          <Button variant="ghost" icon={Zap} busy={testAct.busy} disabled={!canProbe} onClick={runTest}>
            {t('cprof.test')}
          </Button>
          <span className="small muted">{testAct.busy ? t('cprof.testing') : t(agent === 'codex' ? 'xprof.test_hint' : 'cprof.test_hint')}</span>
        </div>
        <ErrorNote error={testAct.error} />
        {testRes && <TestResults agent={agent} data={testRes} catalog={catalog} />}
        <ErrorNote error={error} />
        <div className="modal-actions">
          <Button variant="ghost" onClick={onClose}>
            {t('action.cancel')}
          </Button>
          <Button type="submit" busy={busy} disabled={!ready}>
            {t('action.save')}
          </Button>
        </div>
      </form>
    </Modal>
  );
}

// ---------------- 模型设置（src/model_tuning.rs） ----------------
// 上下文窗口、自动压缩等常用项，按两个助手各自支持的开关来：Claude Code 是启动时的环境变量，Codex 是 config.toml 的键（-c / 会话的 config）。
// 都不填就照它们自己的默认；新建、重新启动的会话生效。

const CODEX_EFFORTS = ['minimal', 'low', 'medium', 'high', 'xhigh', 'max', 'ultra'];
const CODEX_SUMMARIES = ['auto', 'concise', 'detailed', 'none'];
const CODEX_VERBOSITY = ['low', 'medium', 'high'];

/** “400k”“1m”“128000”→ token 数；空的是 null，看不懂的是 NaN */
export function parseTokens(v) {
  const s = String(v ?? '').trim().toLowerCase().replace(/[,_\s]/g, '');
  if (!s) return null;
  const m = /^(\d+(?:\.\d+)?)([km]?)$/.exec(s);
  if (!m) return NaN;
  return Math.round(Number(m[1]) * (m[2] === 'm' ? 1_000_000 : m[2] === 'k' ? 1_000 : 1));
}

/** 400000 → “400K”，1000000 → “1M” */
export function tokensText(n) {
  if (n == null) return '';
  if (n >= 1_000_000 && n % 100_000 === 0) return `${n / 1_000_000}M`;
  if (n >= 1_000 && n % 1_000 === 0) return `${n / 1_000}K`;
  return String(n);
}

/** Antigravity 的自动压缩点 → “800K”“80%” */
function agyCompactText(x) {
  return x?.compact_tokens ? tokensText(x.compact_tokens) : x?.compact_pct ? `${x.compact_pct}%` : '';
}

/** “800K”“80%”→ { compact_tokens } / { compact_pct }；空的是 {}，看不懂的是 null */
export function parseAgyCompact(v) {
  const s = String(v ?? '').trim();
  if (!s) return {};
  const pm = /^(\d+(?:\.\d+)?)\s*%$/.exec(s);
  if (pm) {
    const p = Math.round(Number(pm[1]));
    return p >= 20 && p <= 95 ? { compact_pct: p } : null;
  }
  const n = parseTokens(s);
  return Number.isNaN(n) || n < 100_000 || n > 10_000_000 ? null : { compact_tokens: n };
}

/** Antigravity：只有自动压缩点（AGY_CLI_AUTO_COMPACT_OVERRIDE，起 agy acp 时带上） */
function AgyTuningModal({ p, onClose, onSaved }) {
  const api = useApi();
  const { t } = useTranslation();
  const [v, setV] = useState(() => agyCompactText(p.tuning));
  const { busy, error, run } = useAction();
  const [cur] = useLoad(() => api.get(`/agy-profiles/${p.id}/tuning/current`).catch(() => null), [p.id]);
  const parsed = parseAgyCompact(v);
  const save = (e) => {
    e.preventDefault();
    if (parsed) run(async () => onSaved(await api.put(`/agy-profiles/${p.id}/tuning`, parsed)));
  };
  const reset = () => run(async () => onSaved(await api.put(`/agy-profiles/${p.id}/tuning`, {})));
  const hint = parsed?.compact_tokens ? `= ${parsed.compact_tokens.toLocaleString()} tokens · ${t('tune.agy_compact_hint')}` : t('tune.agy_compact_hint');
  return (
    <Modal open onClose={onClose} title={t('tune.title', { name: profileName(t, p, 'agy') })} wide>
      <form onSubmit={save}>
        <p className="small muted" style={{ marginTop: 0 }}>
          {t('tune.intro_agy')}
        </p>
        {cur?.raw && (
          <Note>
            <b>{t('tune.agy_cur', { v: cur.raw })}</b>
          </Note>
        )}
        <h3 className="tune-head">{t('tune.sec_context')}</h3>
        <div className="cprof-models">
          <Field label={t('tune.compact_limit')} hint={hint} error={parsed ? null : t('tune.agy_compact_range')}>
            <input className="mono" value={v} onChange={(e) => setV(e.target.value)} inputMode="text" spellCheck={false} autoCapitalize="none" placeholder={cur?.raw ? t('tune.from_file', { v: cur.raw }) : t('tune.default')} maxLength={12} />
          </Field>
        </div>
        <p className="small muted">{t('tune.agy_window')}</p>
        <p className="small muted">{t('tune.agy_applies')}</p>
        <ErrorNote error={error} />
        <div className="modal-actions">
          <Button variant="ghost" onClick={reset} disabled={busy}>
            {t('tune.reset')}
          </Button>
          <Button variant="ghost" onClick={onClose}>
            {t('action.cancel')}
          </Button>
          <Button type="submit" busy={busy} disabled={!parsed}>
            {t('action.save')}
          </Button>
        </div>
      </form>
    </Modal>
  );
}

/** 列表里那一行底下的摘要：设了哪些 */
function tuningSummary(t, agent, p) {
  const x = p.tuning;
  if (!x) return [];
  if (agent === 'agy') return x.compact_tokens || x.compact_pct ? [t('tune.sum_compact', { n: agyCompactText(x) })] : [];
  const api = p.kind === 'api';
  const out = [];
  if (x.context_window && (api || agent === 'codex')) out.push(t('tune.sum_window', { n: tokensText(x.context_window) }));
  if (x.no_1m) out.push(t('tune.sum_no_1m'));
  if (x.no_auto_compact) out.push(t('tune.sum_no_compact'));
  else if (x.compact_window || x.compact_limit) out.push(t('tune.sum_compact', { n: tokensText(x.compact_window ?? x.compact_limit) }));
  if (x.max_output) out.push(t('tune.sum_output', { n: tokensText(x.max_output) }));
  if (x.effort) out.push(t('tune.sum_effort', { v: x.effort }));
  return out;
}

function TokenField({ label, hint, value, onChange, placeholder, min, max }) {
  const { t } = useTranslation();
  const n = parseTokens(value);
  const bad = Number.isNaN(n) || (n != null && (n < min || n > max));
  return (
    <Field label={label} hint={n && !bad ? `= ${n.toLocaleString()} tokens${hint ? ` · ${hint}` : ''}` : hint} error={bad ? t('tune.range', { min: tokensText(min), max: tokensText(max) }) : null}>
      <input className="mono" value={value} onChange={(e) => onChange(e.target.value)} inputMode="text" spellCheck={false} autoCapitalize="none" placeholder={placeholder ?? t('tune.default')} maxLength={12} />
    </Field>
  );
}

function CheckRow({ checked, onChange, label, hint, disabled }) {
  return (
    <label className="check-row">
      <input type="checkbox" checked={checked} disabled={disabled} onChange={(e) => onChange(e.target.checked)} />
      <span>
        {label}
        {hint && (
          <span className="small muted" style={{ display: 'block' }}>
            {hint}
          </span>
        )}
      </span>
    </label>
  );
}

/** claude-opus-5-5 → Opus 5.5（只为显示） */
function modelShort(id) {
  const m = /^claude-([a-z]+)-(\d+)(?:-(\d+))?/.exec(String(id ?? ''));
  return m ? `${m[1][0].toUpperCase()}${m[1].slice(1)} ${m[2]}${m[3] ? `.${m[3]}` : ''}` : String(id ?? '');
}

/** 用户自己的配置文件里（settings.json / config.toml）已经设了的：不填时照的就是这些，列出来别让人以为是空的 */
function userTuningLines(t, agent, cur) {
  if (!cur) return [];
  const out = [];
  if (agent === 'codex') {
    if (cur.context_window) out.push(t('tune.cur_window', { n: tokensText(cur.context_window) }));
    if (cur.compact_limit) out.push(t('tune.cur_compact', { n: tokensText(cur.compact_limit) }));
    if (cur.effort) out.push(t('tune.sum_effort', { v: cur.effort }));
    if (cur.summary) out.push(`model_reasoning_summary = ${cur.summary}`);
    if (cur.verbosity) out.push(`model_verbosity = ${cur.verbosity}`);
    return out;
  }
  const c = cur.compact ?? {};
  if (c.enabled === false) out.push(t('tune.cur_compact_off'));
  else {
    for (const m of cur.compact_models ?? []) out.push(m.at ? t('tune.cur_compact_model', { model: modelShort(m.model), n: tokensText(m.at) }) : t('tune.cur_compact_model_auto', { model: modelShort(m.model) }));
    if (c.at && c.source !== 'env') out.push(t('tune.cur_compact', { n: tokensText(c.at) }));
  }
  out.push(c.idle ? t('tune.cur_idle_on', { n: tokensText(c.idle_min) }) : t('tune.cur_idle_off'));
  if (cur.no_1m) out.push(t('tune.sum_no_1m'));
  if (cur.context_window) out.push(t('tune.cur_window', { n: tokensText(cur.context_window) }));
  if (cur.max_output) out.push(t('tune.sum_output', { n: tokensText(cur.max_output) }));
  return out;
}

function TuningModal({ agent, p, onClose, onSaved }) {
  const api = useApi();
  const { t } = useTranslation();
  const cfg = AGENTS[agent];
  // 第三方接口（不能叫 api：会盖住上面引入的 api，保存、读配置时报 api.put is not a function）
  const isApi = p.kind === 'api';
  const x = p.tuning ?? {};
  const tok = (n) => (n ? tokensText(n) : '');
  const [f, setF] = useState(() => ({
    context_window: tok(x.context_window),
    compact: tok(agent === 'codex' ? x.compact_limit : x.compact_window),
    max_output: tok(x.max_output),
    no_1m: Boolean(x.no_1m),
    no_auto_compact: Boolean(x.no_auto_compact),
    always_effort: Boolean(x.always_effort),
    timeout: String((agent === 'codex' ? x.idle_timeout_secs : x.timeout_secs) ?? ''),
    effort: x.effort ?? '',
    summary: x.summary ?? '',
    verbosity: x.verbosity ?? '',
  }));
  const set = (k) => (v) => setF((o) => ({ ...o, [k]: v }));
  const { busy, error, run } = useAction();
  // 用户自己的配置文件里设了什么（读不到就当没有）
  const [cur] = useLoad(() => api.get(`${cfg.base}/${p.id}/tuning/current`).catch(() => null), [agent, p.id]);
  const curLines = userTuningLines(t, agent, cur);
  const curFile =
    agent === 'codex'
      ? cur?.source ?? '~/.codex/config.toml'
      : (cur?.compact?.source !== 'env' && cur?.compact?.source) || cur?.compact_models?.[0]?.source || '~/.claude/settings.json';
  // 不填时输入框里灰字写配置文件里的值
  const fromFile = (n) => (n ? t('tune.from_file', { v: tokensText(n) }) : undefined);
  const perModel = agent === 'claude' && (cur?.compact_models ?? []).some((m) => m.at);
  const win = parseTokens(f.context_window);
  const compact = parseTokens(f.compact);
  const out = parseTokens(f.max_output);
  const timeout = f.timeout.trim() ? Number(f.timeout) : null;
  const inRange = (n, a, b) => n == null || (!Number.isNaN(n) && n >= a && n <= b);
  const timeoutOk = timeout == null || (Number.isInteger(timeout) && timeout >= 10 && timeout <= 3600);
  const codexOrder = agent !== 'codex' || win == null || compact == null || compact < win;
  const ok =
    inRange(win, 8_000, 10_000_000) &&
    (agent === 'codex' ? inRange(compact, 1_000, 10_000_000) : inRange(compact, 100_000, 1_000_000)) &&
    inRange(out, 1_000, 1_000_000) &&
    timeoutOk &&
    codexOrder;
  const body = () =>
    agent === 'codex'
      ? {
          context_window: win,
          compact_limit: compact,
          effort: f.effort || null,
          summary: f.summary || null,
          verbosity: f.verbosity || null,
          idle_timeout_secs: isApi ? timeout : null,
        }
      : {
          context_window: isApi ? win : null,
          no_1m: f.no_1m,
          no_auto_compact: f.no_auto_compact,
          compact_window: compact,
          max_output: out,
          always_effort: isApi && f.always_effort,
          timeout_secs: isApi ? timeout : null,
        };
  const save = (e) => {
    e.preventDefault();
    run(async () => onSaved(await api.put(`${cfg.base}/${p.id}/tuning`, body())));
  };
  const reset = () => run(async () => onSaved(await api.put(`${cfg.base}/${p.id}/tuning`, {})));
  const choose = (k, list, prefix) => (
    <Select value={f[k]} onChange={(e) => set(k)(e.target.value)}>
      <option value="">{cur?.[k] ? t('tune.default_file', { v: t(`${prefix}.${cur[k]}`, { defaultValue: cur[k] }) }) : t('tune.default')}</option>
      {list.map((v) => (
        <option key={v} value={v}>
          {t(`${prefix}.${v}`, { defaultValue: v })}
        </option>
      ))}
    </Select>
  );
  return (
    <Modal open onClose={onClose} title={t('tune.title', { name: profileName(t, p, agent) })} wide>
      <form onSubmit={save}>
        <p className="small muted" style={{ marginTop: 0 }}>
          {t(`tune.intro_${agent}`)}
        </p>
        {curLines.length > 0 && (
          <Note>
            <b>{t('tune.cur_title', { file: curFile })}</b>
            <ul className="tune-cur">
              {curLines.map((l) => (
                <li key={l}>{l}</li>
              ))}
            </ul>
          </Note>
        )}
        <h3 className="tune-head">{t('tune.sec_context')}</h3>
        <div className="cprof-models">
          {(isApi || agent === 'codex') && (
            <TokenField label={t('tune.context_window')} hint={t(`tune.context_window_hint_${agent}`)} value={f.context_window} onChange={set('context_window')} min={8_000} max={10_000_000} placeholder={fromFile(cur?.context_window)} />
          )}
          {agent === 'claude' ? (
            <TokenField
              label={t('tune.compact_window')}
              hint={t('tune.compact_window_hint')}
              value={f.no_auto_compact ? '' : f.compact}
              onChange={set('compact')}
              min={100_000}
              max={1_000_000}
              placeholder={f.no_auto_compact ? t('tune.compact_off') : perModel ? t('tune.from_file_models') : fromFile(cur?.compact?.source !== 'env' ? cur?.compact?.at : null)}
            />
          ) : (
            <TokenField label={t('tune.compact_limit')} hint={t('tune.compact_limit_hint')} value={f.compact} onChange={set('compact')} min={1_000} max={10_000_000} placeholder={fromFile(cur?.compact_limit)} />
          )}
        </div>
        {!codexOrder && <Note kind="warn">{t('tune.compact_over')}</Note>}
        {perModel && compact != null && !Number.isNaN(compact) && !f.no_auto_compact && <Note kind="warn">{t('tune.compact_overrides_models')}</Note>}
        {agent === 'claude' && (
          <>
            <CheckRow checked={f.no_auto_compact} onChange={set('no_auto_compact')} label={t('tune.no_auto_compact')} hint={t('tune.no_auto_compact_hint')} />
            <CheckRow checked={f.no_1m} onChange={set('no_1m')} label={t('tune.no_1m')} hint={t('tune.no_1m_hint')} />
          </>
        )}
        <h3 className="tune-head">{t('tune.sec_output')}</h3>
        {agent === 'claude' ? (
          <>
            <div className="cprof-models">
              <TokenField label={t('tune.max_output')} hint={t('tune.max_output_hint')} value={f.max_output} onChange={set('max_output')} min={1_000} max={1_000_000} />
            </div>
            {isApi && <CheckRow checked={f.always_effort} onChange={set('always_effort')} label={t('tune.always_effort')} hint={t('tune.always_effort_hint')} />}
            {!isApi && <p className="small muted">{t('tune.effort_claude')}</p>}
          </>
        ) : (
          <div className="cprof-models">
            <Field label={t('tune.effort')} hint={t('tune.effort_hint')}>
              {choose('effort', CODEX_EFFORTS, 'tune.effort_v')}
            </Field>
            <Field label={t('tune.summary')} hint={t('tune.summary_hint')}>
              {choose('summary', CODEX_SUMMARIES, 'tune.summary_v')}
            </Field>
            <Field label={t('tune.verbosity')} hint={t('tune.verbosity_hint')}>
              {choose('verbosity', CODEX_VERBOSITY, 'tune.verbosity_v')}
            </Field>
          </div>
        )}
        {isApi && (
          <>
            <h3 className="tune-head">{t('tune.sec_network')}</h3>
            <div className="cprof-models">
              <Field label={t(`tune.timeout_${agent}`)} hint={t(`tune.timeout_${agent}_hint`)} error={timeoutOk ? null : t('tune.timeout_range')}>
                <input className="mono" value={f.timeout} onChange={(e) => set('timeout')(e.target.value.replace(/\D/g, ''))} inputMode="numeric" placeholder={t('tune.default')} maxLength={4} />
              </Field>
            </div>
          </>
        )}
        <p className="small muted">{t('tune.applies')}</p>
        <ErrorNote error={error} />
        <div className="modal-actions">
          <Button variant="ghost" onClick={reset} disabled={busy}>
            {t('tune.reset')}
          </Button>
          <Button variant="ghost" onClick={onClose}>
            {t('action.cancel')}
          </Button>
          <Button type="submit" busy={busy} disabled={!ok}>
            {t('action.save')}
          </Button>
        </div>
      </form>
    </Modal>
  );
}

/**
 * 一个配置。Claude Code 的官方账号：已登录时按钮是“查用量”（请 Claude Code 去取，顺便确认登录状态；登录失效就提示重新登录），
 * 没登录时是“登录”，“重新登录”在更多菜单里。Codex 的本机默认只是说明（Codex 自己的登录和配置）。
 */
function ProfileRow({ agent, p, isDefault, busy, nonce, onDefault, onLogin, onEdit, onTune, onRemove, only }) {
  const api = useApi();
  const { t } = useTranslation();
  const toast = useToast();
  const cfg = AGENTS[agent];
  const official = p.kind === 'official' || agent === 'agy';
  const hasLogin = official && (agent === 'claude' || agent === 'agy');
  const [loaded, err] = useLoad(() => (hasLogin ? api.get(`${cfg.base}/${p.id}/status`) : Promise.resolve(null)), [agent, p.id, nonce]);
  // 查用量时顺便拿到的登录状态比上面问到的新；重新问过之后以问到的为准
  const [fresh, setFresh] = useState(null);
  useEffect(() => setFresh(null), [loaded]);
  const s = fresh ?? loaded;
  const [usage, setUsage] = useState(null);
  const q = useAction();
  const name = profileName(t, p, agent);
  const take = (r) => {
    if (r.status && !r.status.error) setFresh(r.status);
    setUsage(r.state === 'ok' ? { ...r, at: Date.now() } : r.state === 'logged_out' ? { state: 'logged_out' } : null);
  };
  const queryUsage = async () => {
    const r = await q.run(() => api.post(`${cfg.base}/${p.id}/usage`, {}));
    if (!r) return;
    take(r);
    if (r.state === 'logged_out') toast(t('cprof.expired_toast', { name }), 'error');
    else if (r.state === 'no_limits') toast(t(agent === 'agy' ? 'aprof.usage_none' : official ? 'cprof.usage_none' : 'cprof.usage_none_api'), 'error');
    else if (r.state === 'failed') toast(t('cprof.usage_failed'), 'error');
  };
  useEffect(() => {
    if (q.error) toast(errorText(t, q.error), 'error');
  }, [q.error]);
  const loggedIn = Boolean(s?.logged_in);
  // 定时自动查用量：打开页面时查一次，开着时第三方接口每 5 分钟、官方账号每 10 分钟再查；
  // 页面在后台时不查，回到前台时过了间隔就补一次。自动查的不弹提示，失败了保留上次的数
  const autoQuery = hasLogin ? loggedIn || (agent === 'agy' && p.id === 'official' && Boolean(s) && !s.error) : !official && p.quota && p.key_set;
  const every = hasLogin ? 10 * 60_000 : 5 * 60_000;
  useEffect(() => {
    if (!autoQuery) return undefined;
    let alive = true;
    let last = 0;
    const run = async () => {
      if (document.hidden || Date.now() - last < every - 5_000) return;
      last = Date.now();
      try {
        const r = await api.post(`${cfg.base}/${p.id}/usage`, {});
        if (alive && r && r.state !== 'failed' && r.state !== 'no_limits') take(r);
      } catch {
        /* 自动查的：不打扰 */
      }
    };
    // 几个配置错开一点，不同时起好几个子进程
    const first = setTimeout(run, 300 + Math.random() * 1500);
    const iv = setInterval(run, every);
    const vis = () => !document.hidden && run();
    document.addEventListener('visibilitychange', vis);
    return () => {
      alive = false;
      clearTimeout(first);
      clearInterval(iv);
      document.removeEventListener('visibilitychange', vis);
    };
  }, [autoQuery, p.id, agent]);
  const model = agent === 'codex' ? p.model : p.models?.main;
  const tuned = tuningSummary(t, agent, p);
  return (
    <li className={isDefault ? 'cprof-default' : ''}>
      <button
        type="button"
        className="cprof-radio"
        role="radio"
        aria-checked={isDefault}
        disabled={busy}
        onClick={() => !isDefault && onDefault(p)}
        title={isDefault ? t('cprof.is_default') : t('cprof.make_default')}
        aria-label={`${name} · ${isDefault ? t('cprof.is_default') : t('cprof.make_default')}`}
      />
      <span className="grow" style={{ minWidth: 0 }}>
        <b className="cprof-name">
          <KindIcon p={p} />
          <span className="ellipsis">{name}</span>
          {isDefault && <span className="badge badge-ok">{t('cprof.default')}</span>}
        </b>
        <span className="small" style={{ display: 'block' }}>
          {hasLogin ? (
            <LoginStatus s={s} err={err} expired={usage?.state === 'logged_out'} agent={agent} />
          ) : official ? (
            <span className="muted">{t('xprof.builtin_desc')}</span>
          ) : (
            <>
              <span className="mono muted ellipsis">{p.base_url}</span>
              {model && <span className="muted"> · {model}</span>}
              {!p.key_set && (
                <span className="cprof-warn">
                  {' '}
                  · <KeyRound size={13} style={{ verticalAlign: '-2px' }} /> {t('cprof.no_key')}
                </span>
              )}
            </>
          )}
        </span>
        {tuned.length > 0 && (
          <button type="button" className="small muted cprof-tuned" onClick={() => onTune(p)} title={t('tune.menu')}>
            <SlidersHorizontal size={13} aria-hidden="true" /> {tuned.join(' · ')}
          </button>
        )}
        {(loggedIn || !official || agent === 'agy') && usage?.state === 'ok' && (
          <span className="small" style={{ display: 'block' }}>
            <UsageLine limits={usage.limits} />
            {usage.at && <span className="muted"> · {t('cprof.usage_auto_at', { at: new Date(usage.at).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' }) })}</span>}
          </span>
        )}
      </span>
      <span className="inline">
        {hasLogin &&
          (loggedIn || !s ? (
            <Button variant="ghost" icon={Gauge} onClick={queryUsage} busy={q.busy} disabled={busy || !s} title={t(agent === 'agy' ? 'aprof.usage_hint' : 'cprof.usage_hint')}>
              {t('cprof.usage')}
            </Button>
          ) : (
            <Button variant="ghost" icon={LogIn} onClick={() => onLogin(p)} disabled={busy}>
              {t('cprof.login')}
            </Button>
          ))}
        {!official && p.quota && p.key_set && (
          <Button variant="ghost" icon={Gauge} onClick={queryUsage} busy={q.busy} disabled={busy} title={t('cprof.usage_hint_api')}>
            {t('cprof.usage')}
          </Button>
        )}
        {/* Claude Code 的本机默认账号也能改名、从列表里去掉（只剩它一个时不能去掉）；Codex 的本机默认只能改模型设置；Antigravity 的本机默认可改名、不能删除 */}
        <PopMenu
            label={t('action.more')}
            items={[
              hasLogin && loggedIn && { icon: LogIn, label: t('cprof.relogin'), onClick: () => onLogin(p) },
              agent === 'agy' && !loggedIn && s && !s.error && { icon: Gauge, label: t('cprof.usage'), onClick: queryUsage },
              !isDefault && { icon: Check, label: t('cprof.make_default'), onClick: () => onDefault(p) },
              { icon: SlidersHorizontal, label: t('tune.menu'), hint: t(`tune.menu_hint_${agent}`), onClick: () => onTune(p) },
              (p.id !== 'official' || agent === 'claude' || agent === 'agy') && { label: t(p.kind === 'api' ? 'cprof.edit' : 'cprof.rename'), onClick: () => onEdit(p) },
              !(p.id === 'official' && (only || agent === 'codex' || agent === 'agy')) && { label: t(p.id === 'official' ? 'cprof.hide_builtin' : 'action.delete'), danger: true, onClick: () => onRemove(p) },
            ]}
          />
      </span>
    </li>
  );
}

function ProfilesCard({ agent, bare }) {
  const api = useApi();
  const { t } = useTranslation();
  const confirm = useConfirm();
  const toast = useToast();
  const cfg = AGENTS[agent];
  const [data, error, reload] = useLoad(() => api.get(cfg.base), [agent]);
  const [form, setForm] = useState(null);
  // 登录完回来时重新问一遍登录状态
  const [nonce, setNonce] = useState(0);
  const { busy, error: actErr, run } = useAction();
  // 从开始页的“管理”过来时滚到这里
  useEffect(() => {
    if (data && window.location.hash === `#${cfg.anchor}`) document.getElementById(cfg.anchor)?.scrollIntoView({ block: 'start' });
  }, [Boolean(data)]);
  useEffect(() => {
    if (agent !== 'claude' && agent !== 'agy') return undefined;
    const again = () => document.visibilityState === 'visible' && setNonce((n) => n + 1);
    document.addEventListener('visibilitychange', again);
    return () => document.removeEventListener('visibilitychange', again);
  }, [agent]);
  const apply = () => reload();
  // 正在登录的账号（弹出登录框）
  const [loggingIn, setLoggingIn] = useState(null);
  const login = (p) => setLoggingIn(p);
  const setDefault = async (p) => {
    const r = await run(() => api.put(`${cfg.base}/default`, { id: p.id }));
    if (r) {
      apply(r);
      toast(t('cprof.default_set', { name: profileName(t, p, agent) }));
    }
  };
  const remove = async (p) => {
    const builtin = p.id === 'official';
    const ok = await confirm({
      title: t(builtin ? 'cprof.hide_builtin_title' : 'cprof.delete_title', { name: profileName(t, p, agent) }),
      body: t(builtin ? 'cprof.hide_builtin_body' : p.kind === 'api' ? `${cfg.t}.delete_body_api` : `${cfg.t}.delete_body_official`),
      confirm: t(builtin ? 'cprof.hide_builtin' : 'action.delete'),
      danger: true,
    });
    if (!ok) return;
    const r = await run(() => api.del(`${cfg.base}/${p.id}`));
    if (r) apply(r);
  };
  // 添加官方账号：不用先填什么，直接去登录；登录框没登录成功就关掉时删掉这个空账号
  const addOfficial = async () => {
    const r = await run(() => api.post(cfg.base, agent === 'claude' ? { kind: 'official' } : {}));
    const p = r?.profiles?.find((x) => x.id === r.added);
    if (!p) return;
    apply(r);
    setLoggingIn({ ...p, fresh: true });
  };
  const loginClosed = async (ok) => {
    const p = loggingIn;
    setLoggingIn(null);
    if (p?.fresh && !ok) {
      const r = await api.del(`${cfg.base}/${p.id}`).catch(() => null);
      if (r) apply(r);
    } else reload();
    setNonce((n) => n + 1);
  };
  const saved = (r) => {
    setForm(null);
    apply(r);
    toast(t('cprof.saved'));
  };
  const add =
    agent === 'claude' ? (
      <PopMenu
        className="btn btn-secondary"
        icon={Plus}
        text={t('cprof.add')}
        label={t('cprof.add')}
        items={[
          { icon: UserRound, label: t('cprof.add_official'), hint: t('cprof.add_official_hint'), onClick: addOfficial },
          { icon: Server, label: t('cprof.add_api'), hint: t('cprof.add_api_hint'), onClick: () => setForm({ kind: 'api' }) },
        ]}
      />
    ) : agent === 'agy' ? (
      <Button variant="secondary" icon={Plus} onClick={addOfficial} disabled={busy}>
        {t('aprof.add_official')}
      </Button>
    ) : (
      <Button variant="secondary" icon={Plus} onClick={() => setForm({ kind: 'api' })}>
        {t('xprof.add_api')}
      </Button>
    );
  return (
    <div id={bare ? undefined : cfg.anchor} className={bare ? 'cprof-bare' : 'anchor'}>
      <Shell bare={bare} title={t(`${cfg.t}.title`)} actions={data && add}>
        <p className="muted small">{t(`${cfg.t}.intro`)}</p>
        <ErrorNote error={error ?? actErr} />
        {!data && !error && <Skeleton height={96} />}
        {data && (
          <ul className="list cprof-list">
            {data.profiles.map((p) => (
              <ProfileRow
                key={p.id}
                agent={agent}
                p={p}
                isDefault={p.id === data.default}
                busy={busy}
                nonce={nonce}
                onDefault={setDefault}
                onLogin={login}
                onEdit={(x) => setForm({ kind: x.kind || 'official', initial: x })}
                onTune={(x) => setForm({ kind: 'tune', initial: x })}
                onRemove={remove}
                only={data.profiles.length === 1}
              />
            ))}
          </ul>
        )}
        {agent === 'agy' && data && data.profiles.length > 1 && (
          <label className="row-check small" style={{ display: 'flex', gap: 6, alignItems: 'flex-start', margin: '12px 0 0' }}>
            <input
              type="checkbox"
              checked={!!data.auto_switch}
              disabled={busy}
              onChange={(e) =>
                api
                  .put(`${cfg.base}/auto-switch`, { on: e.target.checked })
                  .then(apply)
                  .catch((x) => toast(errorText(t, x), 'error'))
              }
            />
            <span>{t('aprof.auto_switch')}</span>
          </label>
        )}
        <p className="muted small" style={{ margin: '12px 0 0' }}>
          {t(`${cfg.t}.note`)}
        </p>
      </Shell>
      {loggingIn && <LoginModal agent={agent} profile={loggingIn} onClose={loginClosed} />}
      {form?.kind === 'tune' &&
        (agent === 'agy' ? <AgyTuningModal p={form.initial} onClose={() => setForm(null)} onSaved={saved} /> : <TuningModal agent={agent} p={form.initial} onClose={() => setForm(null)} onSaved={saved} />)}
      {form?.kind === 'official' && <OfficialForm agent={agent} initial={form.initial} onClose={() => setForm(null)} onSaved={saved} />}
      {form?.kind === 'api' && data && <ApiForm agent={agent} initial={form.initial} accepted={Boolean(data.accepted_at)} onClose={() => setForm(null)} onSaved={saved} />}
    </div>
  );
}

/** 设置页里是一张卡片；弹窗里（`bare`）不要卡片的边框，标题由弹窗给，“添加”按钮放在列表上面 */
function Shell({ bare, title, actions, children }) {
  if (!bare) return <Card title={title} actions={actions}>{children}</Card>;
  return (
    <>
      {actions && <div className="cprof-bare-actions">{actions}</div>}
      {children}
    </>
  );
}

/** 对话页的“账号”弹窗用：列表、登录、添加、重新登录，跟设置页同一套 */
export function ProfilesList({ agent }) {
  return <ProfilesCard agent={agent} bare />;
}

export default function ClaudeProfilesCard() {
  return <ProfilesCard agent="claude" />;
}

export function CodexProfilesCard() {
  return <ProfilesCard agent="codex" />;
}

export function AgyProfilesCard() {
  return <ProfilesCard agent="agy" />;
}

/**
 * 新建会话时选用哪个配置（开始页、新建终端表单）。`value` 为 null 表示用默认。
 * 那台电脑的客户端太旧（没有这个接口）、或者只有本机默认时什么都不显示。
 */
function ProfilePick({ agent, client, device, value, onChange, className = 'btn btn-plain start-adv' }) {
  const { t } = useTranslation();
  const { navigate } = useRouter();
  const cfg = AGENTS[agent];
  const [data] = useLoad(() => client.get(cfg.base).catch(() => null), [device, agent]);
  // 换了电脑：另选的那个在新电脑上没有就回到默认
  useEffect(() => {
    if (data && value && value !== 'auto' && !data.profiles.some((p) => p.id === value)) onChange(null);
  }, [data]);
  if (!data || data.profiles.length < 2) return null;
  const def = data.profiles.find((p) => p.id === data.default) ?? data.profiles[0];
  // agy 有好几个账号时可以选“自动”：新建时挑额度最多的那个，之后一直用它
  const auto = agent === 'agy' && value === 'auto';
  const cur = data.profiles.find((p) => p.id === value) ?? def;
  return (
    <PopMenu
      className={className}
      icon={auto ? Sparkles : (p) => <KindIcon p={cur} {...p} />}
      text={auto ? t('aprof.auto_short') : profileName(t, cur, agent)}
      label={t(`${cfg.t}.pick_label`)}
      items={[
        ...(agent === 'agy' ? [{ icon: Sparkles, label: t('aprof.auto'), hint: t('aprof.auto_hint'), checked: auto, onClick: () => onChange('auto') }] : []),
        ...data.profiles.map((p) => ({
          icon: (x) => <KindIcon p={p} {...x} />,
          label: profileName(t, p, agent),
          hint: p.id === def.id ? t('cprof.default') : p.kind === 'api' ? p.base_url : undefined,
          checked: !auto && p.id === cur.id,
          onClick: () => onChange(p.id === def.id ? null : p.id),
        })),
        'sep',
        { label: t(`${cfg.t}.manage`), onClick: () => navigate(`/ai#${cfg.anchor}`) },
      ]}
    />
  );
}

export function ClaudeProfilePick(props) {
  return <ProfilePick agent="claude" {...props} />;
}

export function CodexProfilePick(props) {
  return <ProfilePick agent="codex" {...props} />;
}

export function AgyProfilePick(props) {
  return <ProfilePick agent="agy" {...props} />;
}
