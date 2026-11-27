// 连着的电脑实际走的路与往返时间（电脑菜单每行右侧的色点）：
// 绿 = 局域网直连，蓝 = 公网直连，黄 = 中转，灰 = 还没连（不会为了显示它去建连接）。
// 网页（WebRTC）由启动页的 PeerConnection 统计给出（server-web/rtc/boot.js 的 __llrtc.paths）；
// 本机 App 连其他电脑由客户端给出（GET /api/rtc/paths，src/rtc/outbound.rs）。本机自己不走 WebRTC，只写“本机”。
import { RefreshCw } from 'lucide-react';
import { useEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';

import { localApi, rtcMode } from './api.js';
import { connManaged, isOnline, reconnectDevice, useConn, useDeviceConn } from './conn.js';

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

export const refreshPaths = load;

function tick() {
  if (document.visibilityState === 'visible') load();
}

/** 订阅路径数据：有人在看时每 10 秒刷新一次，页面在后台时不刷新。按每台电脑自己的连接状态决定是否置空延迟（互不影响）。 */
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
  const conn = useConn();
  if (!connManaged) return data;
  if (conn.links) {
    return Object.fromEntries(
      Object.entries(data).map(([k, v]) => {
        const ph = conn.links[k]?.phase;
        const ok = !ph || ph === 'online' || ph === 'idle';
        return [k, ok ? v : { ...v, rtt_ms: null, loss: null }];
      }),
    );
  }
  if (!isOnline(conn)) return Object.fromEntries(Object.entries(data).map(([k, v]) => [k, { ...v, rtt_ms: null, loss: null }]));
  return data;
}

/** 一台电脑的路径：`{kind, rtt_ms, loss, local, remote}`；没连着为 null。`loss` 是 0–1 的丢包率，对端是旧版或样本还少时为 null。 */
export function usePath(device) {
  const all = usePaths();
  const conn = useDeviceConn(device);
  const p = (device && all[device]) || null;
  if (!p) return null;
  if (connManaged && !isOnline(conn)) return { ...p, rtt_ms: null, loss: null };
  return p;
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

// 走中转时，按那台电脑自己测的网络类型（docs/08 的 reach）说一句为什么没直连；其余档位不说
const RELAY_REACH = new Set(['relay_only', 'wrong_addr', 'partial', 'proxy']);

function ReconnectButton({ device, isRelay }) {
  const { t } = useTranslation();
  const [busy, setBusy] = useState(false);
  if (!rtcMode || !device || typeof window.top.__llrtc?.reconnect !== 'function') return null;
  const onClick = async () => {
    setBusy(true);
    try {
      await reconnectDevice(device);
      await load();
    } finally {
      setBusy(false);
    }
  };
  return (
    <button type="button" className="btn btn-plain btn-sm conn-reconnect-btn" disabled={busy} onClick={onClick}>
      <RefreshCw size={12} className={busy ? 'spin' : undefined} aria-hidden="true" />
      <span>{busy ? t('path.reconnecting_now') : t(isRelay ? 'path.try_direct' : 'path.reconnect')}</span>
    </button>
  );
}

/** 连接的详情（放在 `dl.kv` 里）：延迟、丢包率、连接方式（带一句说明）、两端地址。`ms` 不给时用路径自己的往返时间。 */
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
            <div className="conn-path-head">
              <span>
                <span className={`dot path-${p.kind}`} aria-hidden="true" style={{ display: 'inline-block', width: 7, height: 7, marginRight: 6 }} />
                {t(`path.${p.kind}`)}
              </span>
              <ReconnectButton device={device} isRelay={p.kind === 'relay'} />
            </div>
            <span className="muted small" style={{ display: 'block' }}>
              {t(`path.${p.kind}_hint`)}
            </span>
            {p.kind === 'relay' && RELAY_REACH.has(p.reach) && (
              <span className="small" style={{ display: 'block', color: 'var(--warn)' }}>
                {t(`path.reach.${p.reach}`)} {t('path.reach.see_settings')}
              </span>
            )}
            {(p.local || p.remote) && (
              <span className="muted small mono" style={{ display: 'block', wordBreak: 'break-all' }}>
                {p.local ? `${p.local} ⟷ ${p.remote}` : p.remote}
              </span>
            )}
          </dd>
        </>
      )}
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

function mapPhaseToState(phase) {
  if (phase === 'failed' || phase === 'offline') return 'failed';
  if (phase === 'connecting' || phase === 'boot') return 'connecting';
  if (phase === 'checking' || phase === 'resuming' || phase === 'reconnecting') return 'reconnecting';
  return 'online';
}

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

/** 订阅各台电脑的连接状态：`{设备 id: {s, reason, path}}`；新版启动页实时推送，旧版启动页每 2 秒轮询。 */
export function useLinks() {
  const [, setN] = useState(0);
  const conn = useConn();
  useEffect(() => {
    if (!rtcMode || conn.links) return undefined;
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
  }, [Boolean(conn.links)]);
  const mapped = useMemo(() => {
    if (!conn.links) return null;
    const out = {};
    for (const [id, l] of Object.entries(conn.links)) {
      out[id] = { s: mapPhaseToState(l.phase), reason: l.reason || '', path: l.path || '' };
    }
    return out;
  }, [conn.links]);
  if (!rtcMode) return links;
  if (mapped) return mapped;
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
