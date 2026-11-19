// 连着的电脑实际走的路与往返时间（电脑菜单每行右侧的色点）：
// 绿 = 局域网直连，蓝 = 公网直连，黄 = 中转，灰 = 还没连（不会为了显示它去建连接）。
// 网页（WebRTC）由启动页的 PeerConnection 统计给出（server-web/rtc/boot.js 的 __llrtc.paths）；
// 本机 App 连其他电脑由客户端给出（GET /api/rtc/paths，src/rtc/outbound.rs）。本机自己不走 WebRTC，只写“本机”。
import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';

import { localApi, rtcMode } from './api.js';
import { connManaged, isOnline, useConn } from './conn.js';

const POLL_MS = 10_000;
let data = {};
let timer = 0;
let users = 0;
const subs = new Set();

async function load() {
  try {
    if (rtcMode) data = (await window.top.__llrtc?.paths?.()) ?? {};
    else data = (await localApi.get('/rtc/paths'))?.items ?? {};
  } catch {
    /* 拿不到就保留上一次的 */
  }
  subs.forEach((f) => f());
}

function tick() {
  if (document.visibilityState === 'visible') load();
}

/** 订阅路径数据：有人在看时每 10 秒刷新一次，页面在后台时不刷新。 */
export function usePaths() {
  const [, setN] = useState(0);
  useEffect(() => {
    const f = () => setN((n) => n + 1);
    subs.add(f);
    if (users++ === 0) {
      load();
      timer = setInterval(tick, POLL_MS);
      document.addEventListener('visibilitychange', tick);
    }
    return () => {
      subs.delete(f);
      if (--users === 0) {
        clearInterval(timer);
        document.removeEventListener('visibilitychange', tick);
      }
    };
  }, []);
  // 连接还没确认（切回前台、重连中）：往返时间是旧的，不显示（“—”），免得以为连着（docs/REMOTE_UX_2026-11-07.md §5.2）
  const conn = useConn();
  if (connManaged && !isOnline(conn)) return Object.fromEntries(Object.entries(data).map(([k, v]) => [k, { ...v, rtt_ms: null, loss: null }]));
  return data;
}

/** 一台电脑的路径：`{kind, rtt_ms, loss, local, remote}`；没连着为 null。`loss` 是 0–1 的丢包率，对端是旧版或样本还少时为 null。 */
export function usePath(device) {
  const all = usePaths();
  return (device && all[device]) || null;
}

/** 丢包率的文字（“2%”）；没有数据为 null。 */
export function lossText(loss) {
  return loss == null ? null : `${Math.round(loss * 100)}%`;
}

/** 丢包率的严重程度：5% 起偏高（warn），15% 起很差（bad），其余 ok；没有数据为 null。 */
export function lossLevel(loss) {
  if (loss == null) return null;
  return loss >= 0.15 ? 'bad' : loss >= 0.05 ? 'warn' : 'ok';
}

/** 点开的小弹层：点外面、按 Esc 关 */
export function usePop() {
  const [open, setOpen] = useState(false);
  const ref = useRef(null);
  useEffect(() => {
    if (!open) return undefined;
    const close = (e) => {
      if (e.type === 'keydown' ? e.key === 'Escape' : !ref.current?.contains(e.target)) setOpen(false);
    };
    document.addEventListener('pointerdown', close);
    document.addEventListener('keydown', close);
    return () => {
      document.removeEventListener('pointerdown', close);
      document.removeEventListener('keydown', close);
    };
  }, [open]);
  return { open, setOpen, ref };
}

// 连这台电脑“只直连”：不用中转，本地端口第一个包就发往那台电脑，开着 Clash/mihomo TUN 时按那台电脑 IP 写的代理规则
// 才生效（平时第一个包是发往中转的 STUN，端口被定成直连）；连不上自动改按平常连。
// 网页记在浏览器里（启动页 __llrtc.directOnly），本机 App 记在本机（PUT /api/rtc/direct-only）；按电脑分开记。
let directOnlyList = null;
async function directOnlyGet(device) {
  if (rtcMode) {
    const r = window.top.__llrtc;
    return typeof r?.directOnly === 'function' ? Boolean(r.directOnly(device)) : null;
  }
  directOnlyList ??= (await localApi.get('/rtc/direct-only'))?.items ?? [];
  return directOnlyList.includes(device);
}

function DirectOnlyRow({ device, fellBack }) {
  const { t } = useTranslation();
  const [on, setOn] = useState(null);
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    let alive = true;
    directOnlyGet(device)
      .then((v) => alive && setOn(v))
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [device]);
  if (on == null) return null;
  const toggle = async (e) => {
    const v = e.target.checked;
    setBusy(true);
    try {
      // 网页：启动页记下后整页重连；本机 App：断开到这台电脑的连接，下个请求按新设置连
      if (rtcMode) window.top.__llrtc.setDirectOnly(device, v);
      else {
        directOnlyList = (await localApi.put('/rtc/direct-only', { device, on: v }))?.items ?? null;
        setOn(v);
        load();
      }
    } finally {
      setBusy(false);
    }
  };
  return (
    <>
      <dt>{t('path.direct_only')}</dt>
      <dd>
        <label style={{ display: 'flex', alignItems: 'center', gap: 6 }}>
          <input type="checkbox" checked={on} disabled={busy} onChange={toggle} />
          {t('path.direct_only_label')}
        </label>
        {on && fellBack && (
          <span className="small" style={{ display: 'block', color: 'var(--warn)' }}>
            {t('path.direct_only_fell_back')}
          </span>
        )}
        <span className="muted small" style={{ display: 'block' }}>
          {t('path.direct_only_hint')}
        </span>
      </dd>
    </>
  );
}

// 填了公网地址后在输入框下面小字问一句要不要“只直连”（不弹窗、能关掉，关掉后不再问）。
// 正经由 WebRTC 连着这台电脑（网页远程，或本机 App 连其他电脑）就能直接开；在它本机上改的，只说去哪里开。
const NUDGE_KEY = 'looklook.direct-only-nudge-off';
export function DirectOnlyNudge({ device }) {
  const { t } = useTranslation();
  const all = usePaths();
  const connected = Boolean(device && all[device]);
  const [on, setOn] = useState(null);
  const [off, setOff] = useState(() => {
    try {
      return localStorage.getItem(NUDGE_KEY) === '1';
    } catch {
      return false;
    }
  });
  useEffect(() => {
    if (!connected) return undefined;
    let alive = true;
    directOnlyGet(device)
      .then((v) => alive && setOn(v))
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [device, connected]);
  if (off || on) return null;
  const dismiss = () => {
    try {
      localStorage.setItem(NUDGE_KEY, '1');
    } catch {}
    setOff(true);
  };
  const enable = async () => {
    if (rtcMode) window.top.__llrtc.setDirectOnly(device, true);
    else {
      directOnlyList = (await localApi.put('/rtc/direct-only', { device, on: true }))?.items ?? null;
      setOn(true);
      load();
    }
  };
  return (
    <span className="field-hint" style={{ display: 'block' }}>
      {connected && on === false ? (
        <>
          {t('path.direct_only_nudge')}{' '}
          <button type="button" className="link-button" onClick={enable}>
            {t('path.direct_only_enable')}
          </button>
        </>
      ) : (
        t('path.direct_only_nudge_elsewhere')
      )}{' '}
      <button type="button" className="link-button muted" onClick={dismiss} aria-label={t('path.direct_only_nudge_off')} title={t('path.direct_only_nudge_off')}>
        ×
      </button>
    </span>
  );
}

/** 连接的详情（放在 `dl.kv` 里）：延迟、丢包率、连接方式（带一句说明）、两端地址。`ms` 不给时用路径自己的往返时间。
 * `device`：给了就加“只直连”开关。 */
export function PathRows({ p, ms = p?.rtt_ms ?? null, device = null }) {
  const { t } = useTranslation();
  return (
    <>
      <dt>{t('conn.latency')}</dt>
      <dd>{ms == null ? '—' : `${ms} ms`}</dd>
      <dt>{t('conn.loss')}</dt>
      <dd className={p?.loss != null ? `loss-${lossLevel(p.loss)}` : undefined}>{lossText(p?.loss) ?? '—'}</dd>
      {p && (
        <>
          <dt>{t('path.label')}</dt>
          <dd>
            <span className={`dot path-${p.kind}`} aria-hidden="true" style={{ display: 'inline-block', width: 7, height: 7, marginRight: 6 }} />
            {t(`path.${p.kind}`)}
            <span className="muted small" style={{ display: 'block' }}>
              {t(`path.${p.kind}_hint`)}
            </span>
            {(p.local || p.remote) && (
              <span className="muted small mono" style={{ display: 'block', wordBreak: 'break-all' }}>
                {p.local} ⟷ {p.remote}
              </span>
            )}
          </dd>
        </>
      )}
      {device && <DirectOnlyRow device={device} fellBack={Boolean(p?.direct_fallback)} />}
    </>
  );
}

function pathTip(t, p) {
  if (!p) return t('path.none');
  const loss = lossText(p.loss);
  return [t(`path.${p.kind}`), p.remote?.split(' ')[0], p.rtt_ms != null ? `${p.rtt_ms} ms` : null, loss ? t('path.loss', { loss }) : null].filter(Boolean).join(' · ');
}

/** 电脑菜单每行右侧：色点加延迟；悬停看完整说明（手机上看行里的 [PathLine]）。`self`：本机 App 里的本机，只写“本机”。 */
export function PathDot({ device, self = false }) {
  const { t } = useTranslation();
  const p = usePath(self ? null : device);
  if (self) return <span className="path-dot muted">{t('path.self')}</span>;
  const kind = p?.kind ?? 'none';
  const ms = p?.rtt_ms;
  const tip = pathTip(t, p);
  // 丢包偏高时跟在延迟后面写出来（只看延迟会以为连接很好）
  const bad = lossLevel(p?.loss);
  return (
    <span className="path-dot" title={tip} aria-label={tip}>
      <span className={`dot path-${kind}`} aria-hidden="true" />
      {ms != null ? `${ms} ms` : '—'}
      {bad && bad !== 'ok' && <span className={`loss-${bad}`}> · {t('path.loss', { loss: lossText(p.loss) })}</span>}
    </span>
  );
}

/** 电脑菜单行里的第三行：走的路、丢包率、对端地址。手机没有悬停，靠这一行看；没连着不显示。 */
export function PathLine({ device }) {
  const { t } = useTranslation();
  const p = usePath(device);
  if (!p) return null;
  const loss = lossText(p.loss);
  const addr = p.remote?.split(' ')[0];
  // 地址可能很长（IPv6），放最后，窄屏截断时先截它
  return (
    <span className="small muted path-line">
      {t(`path.${p.kind}`)}
      {' · '}
      <span className={loss ? `loss-${lossLevel(p.loss)}` : undefined}>{t('path.loss', { loss: loss ?? '—' })}</span>
      {addr && ` · ${addr}`}
    </span>
  );
}

// 网页（WebRTC）里到各台电脑的连接状态（boot.js 的 __llrtc.links）。平台只说电脑在不在线（它与平台的长连接），
// 网页自己连不连得上要看这里：连接中 / 在线 / 重连中 / 连不上。本机 App 没有这一层，只看平台的在线。
const LINK_POLL_MS = 2000;
let links = {};
let linkTimer = 0;
let linkUsers = 0;
const linkSubs = new Set();

function loadLinks() {
  if (document.visibilityState !== 'visible') return;
  let next = {};
  try {
    next = window.top.__llrtc?.links?.() ?? {};
  } catch {
    /* 旧的启动页没有 links */
  }
  if (JSON.stringify(next) === JSON.stringify(links)) return;
  links = next;
  linkSubs.forEach((f) => f());
}

/** 订阅各台电脑的连接状态：`{设备 id: {s, reason, path}}`，有人在看时每 2 秒刷新。 */
export function useLinks() {
  const [, setN] = useState(0);
  useEffect(() => {
    if (!rtcMode) return undefined;
    const f = () => setN((n) => n + 1);
    linkSubs.add(f);
    if (linkUsers++ === 0) {
      loadLinks();
      linkTimer = setInterval(loadLinks, LINK_POLL_MS);
    }
    return () => {
      linkSubs.delete(f);
      if (--linkUsers === 0) clearInterval(linkTimer);
    };
  }, []);
  return links;
}

/**
 * 一台电脑给用户看的状态：`offline`（平台说它不在线）、`connecting`、`reconnecting`、`failed`（连不上），
 * 其余为 `online`（含还没去连的）。
 */
export function linkState(device, all) {
  if (!device?.online) return 'offline';
  return all[device.id]?.s ?? 'online';
}

const STATE_KEY = { offline: 'devices.offline', online: 'devices.online', connecting: 'devices.connecting', reconnecting: 'devices.reconnecting', failed: 'devices.unreachable' };

/** 状态的文字（“在线”“连接中…”“连不上”等）。 */
export function linkLabel(t, device, all) {
  return t(STATE_KEY[linkState(device, all)]);
}

/** 连不上时的原因（error 文案里的 RTC_* 等）与重试。 */
export function linkFailure(device, all) {
  const l = linkState(device, all) === 'failed' ? all[device.id] : null;
  if (!l) return null;
  return { reason: l.reason || '', retry: () => window.top.__llrtc?.retry?.(device.id) };
}
