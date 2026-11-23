// 远程通道连接状态：连接中（动画）/ 已连接（到线路 UDP 不通时注明远程控制台改经 TCP）/
// 已连接但到线路的 UDP 和 TCP 都不通（远程控制台连不上）/ 失败（给出原因、怎么办，以及可展开的技术详情）。
// ConnChip：管理台右上角的连接胶囊，本机、局域网、远程打开都在同一个位置：
//   本机：“本机 · 远程在线”，点开是远程通道的详情、“立即重连”和换线路；
//   远程（浏览器，或 App 里选中了别的电脑）：“远程 · 86 ms”，点开是连着哪台电脑、端到端加密、延迟和线路；
//   局域网：“局域网”，点开说明是直接连到这台电脑。
import { Globe, Lock, RefreshCw } from 'lucide-react';
import { Fragment, useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';

import { useStatus } from '../App.jsx';
import { api, deviceApi, rtcMode } from './api.js';
import { connManaged, isOnline, reconnectDevice, retryNow, useDeviceConn, useFocusedDevice } from './conn.js';
import { deviceName, useDevices } from './devices.jsx';
import { linkLabel, linkState, lossLevel, lossText, PathRows, refreshPaths, useLinks, usePaths, usePop } from './paths.jsx';
import { errorText } from './i18n.js';
import { RELAYS_ANCHOR } from './RelayPicker.jsx';
import { Link } from './router.jsx';
import { relativeTime } from './time.js';
import { Button, useToast } from './ui.jsx';

const KNOWN = ['dns', 'refused', 'timeout', 'network', 'auth', 'handshake', 'heartbeat', 'config'];

/** 远程通道连着，但测速发现这台电脑到当前线路的 UDP 和 TCP 都不通：远程控制台（WebRTC）连不上。 */
function udpBlocked(status) {
  return status.relay?.state === 'connected' && !!status.relay_check?.blocked;
}

export default function RelayStatus() {
  const { t } = useTranslation();
  const { status } = useStatus();
  const [open, setOpen] = useState(false);
  const r = status.relay;
  if (!r || r.state === 'off') return null;
  const failed = r.state === 'failed';
  const blocked = udpBlocked(status);
  const tcp = r.state === 'connected' && !blocked && !!status.relay_check?.tcp;
  // 能换的线路：测得出延迟、不是当前这条
  const others = (status.relay_check?.items ?? []).filter((i) => i.latency_ms != null && i.name !== status.relay_check?.current);
  const reason = KNOWN.includes(r.reason) ? r.reason : 'other';
  return (
    <div className={`relay-status relay-${r.state}`} role="status" aria-live="polite">
      <div className="relay-line">
        {r.state === 'connecting' ? (
          <span className="dots" aria-hidden="true">
            <i />
            <i />
            <i />
          </span>
        ) : (
          <span className={`dot ${blocked ? 'warn' : r.state === 'connected' ? 'on' : 'bad'}`} aria-hidden="true" />
        )}
        <b>{t(blocked ? 'relay.state.blocked' : `relay.state.${r.state}`)}</b>
        {failed && r.failures > 1 && <span className="muted small">{t('relay.retrying', { n: r.failures })}</span>}
      </div>
      {blocked && (
        <>
          <p className="relay-why">{t('relay.blocked_why', { name: status.relay_check.current })}</p>
          <p className="muted small relay-local">
            {others.length > 0 ? (
              <>
                {t('relay.blocked_fix')} <Link to={`/settings#${RELAYS_ANCHOR}`}>{t('relays.change_in_settings')}</Link>
              </>
            ) : (
              t('relay.blocked_fix_single')
            )}
          </p>
        </>
      )}
      {tcp && <p className="muted small relay-local">{t('relay.tcp_note', { name: status.relay_check.current })}</p>}
      {failed && (
        <>
          <p className="relay-why">{t(`relay.reason.${reason}`)}</p>
          <p className="muted small relay-local">{t('relay.local_ok')}</p>
          {r.detail && (
            <>
              <button type="button" className="link-btn" aria-expanded={open} onClick={() => setOpen(!open)}>
                {t(open ? 'relay.hide_detail' : 'relay.show_detail')}
              </button>
              {open && (
                <pre className="relay-detail mono small">
                  {r.detail}
                  {'\n'}
                  {t('relay.since', { when: relativeTime(t, new Date(r.since).toISOString()) })}
                </pre>
              )}
            </>
          )}
        </>
      )}
    </div>
  );
}

/** 往返时间（毫秒）：每 10 秒请求一次这台电脑的 /api/ping，取最近 3 次的中位数，免得一次抖动就跳。 */
export function useRemoteLatency(enabled, device = null) {
  const [ms, setMs] = useState(null);
  useEffect(() => {
    if (!enabled) {
      setMs(null);
      return undefined;
    }
    const samples = [];
    let alive = true;
    const client = device ? deviceApi(device) : api;
    const once = async () => {
      if (document.hidden) return;
      const t0 = performance.now();
      try {
        await client.get('/ping');
      } catch (e) {
        if (e.status === 0 || e.status >= 500) {
          if (alive) setMs(null);
          return;
        }
        // 旧版客户端没有这个接口（404）也是一次完整的往返
      }
      samples.push(performance.now() - t0);
      if (samples.length > 3) samples.shift();
      const sorted = [...samples].sort((a, b) => a - b);
      if (alive) setMs(Math.round(sorted[Math.floor(sorted.length / 2)]));
    };
    once();
    const id = setInterval(once, 10000);
    window.addEventListener('focus', once);
    return () => {
      alive = false;
      clearInterval(id);
      window.removeEventListener('focus', once);
    };
  }, [enabled, device]);
  return ms;
}

const SLOW_MS = 150;
const VERY_SLOW_MS = 400;

/** 点开胶囊、点外面或按 Esc 关上。 */
/** 本机打开：远程通道的状态、重连、换线路。 */
function LocalPop() {
  const { t } = useTranslation();
  const { status, reload } = useStatus();
  const toast = useToast();
  const [busy, setBusy] = useState(false);
  const r = status.relay;
  const reconnect = async () => {
    setBusy(true);
    try {
      await api.post('/relay/reconnect');
      toast(t('relay.reconnecting'));
      setTimeout(reload, 1500);
    } catch (e) {
      toast(errorText(t, e), 'error');
    } finally {
      setBusy(false);
    }
  };
  return (
    <>
      <b className="conn-pop-title">{t('conn.local_title')}</b>
      <RelayStatus />
      {r.state === 'connected' && r.since > 0 && <p className="muted small" style={{ margin: 0 }}>{t('relay.connected_since', { when: relativeTime(t, new Date(r.since).toISOString()) })}</p>}
      {status.relay_check?.current && (
        <p className="small" style={{ margin: 0 }}>
          {t('conn.route', { name: status.relay_check.current })} · <Link to={`/settings#${RELAYS_ANCHOR}`}>{t('conn.change_route')}</Link>
        </p>
      )}
      <div className="relay-pop-actions">
        <Button variant="ghost" icon={RefreshCw} busy={busy} onClick={reconnect}>
          {t('relay.reconnect')}
        </Button>
      </div>
      <p className="muted small" style={{ margin: 0 }}>{t('conn.local_note')}</p>
    </>
  );
}

/** 远程打开：连着哪台电脑、加密、延迟、线路，以及所有在线电脑各自独立状态。 */
function RemotePop({ ms, name, path, device }) {
  const { t } = useTranslation();
  const { status } = useStatus();
  const devices = useDevices();
  const paths = usePaths();
  const links = useLinks();
  const [reconnectingId, setReconnectingId] = useState(null);
  const onlineList = rtcMode && devices.multi ? devices.items.filter((d) => d.online) : [];
  return (
    <>
      <b className="conn-pop-title">{t('conn.remote_title', { name })}</b>
      <dl className="kv conn-kv">
        <dt>{t('conn.secure')}</dt>
        <dd className="ok-text">
          <Lock size={13} aria-hidden="true" /> {t('conn.e2e')}
        </dd>
        <PathRows p={path} ms={ms} device={device} />
        {status.relay_check?.current && (
          <>
            <dt>{t('conn.route_label')}</dt>
            <dd>
              {status.relay_check.current} · <Link to={`/settings#${RELAYS_ANCHOR}`}>{t('conn.change_route_there')}</Link>
            </dd>
          </>
        )}
      </dl>
      {onlineList.length > 1 && (
        <div className="conn-dev-list">
          <span className="muted small conn-dev-head">{t('conn.all_devices')}</span>
          {onlineList.map((d) => {
            const st = linkState(d, links);
            const p = paths[d.id];
            const dotCls = st === 'failed' ? 'bad' : st === 'connecting' || st === 'reconnecting' ? 'wait' : p ? `path-${p.kind}` : 'path-none';
            const subText = st === 'online' ? (p ? [t(`path.${p.kind}`), p.rtt_ms != null ? `${p.rtt_ms} ms` : null].filter(Boolean).join(' · ') : t('devices.online')) : linkLabel(t, d, links);
            const busy = reconnectingId === d.id;
            const reason = st === 'failed' ? links[d.id]?.reason || '' : '';
            return (
              <Fragment key={d.id}>
              <div className={`conn-dev-row ${d.id === device ? 'on' : ''}`}>
                <span className={`dot ${dotCls}`} aria-hidden="true" />
                <span className="conn-dev-name ellipsis">{deviceName(t, d)}</span>
                <span className="conn-dev-meta">{subText}</span>
                <button
                  type="button"
                  className="btn btn-plain btn-icon conn-dev-btn"
                  disabled={busy}
                  title={t(st === 'failed' ? 'devices.retry' : 'path.reconnect')}
                  aria-label={t(st === 'failed' ? 'devices.retry' : 'path.reconnect')}
                  onClick={async () => {
                    setReconnectingId(d.id);
                    try {
                      if (st === 'failed') await retryNow(d.id);
                      else await reconnectDevice(d.id);
                      await refreshPaths();
                    } finally {
                      setReconnectingId(null);
                    }
                  }}
                >
                  <RefreshCw size={12} className={busy ? 'spin' : undefined} />
                </button>
              </div>
              {/* 连不上：写明原因（公网 HTTPS 直连不通时不会回退 WebRTC，靠这里和右上角的提示告诉用户） */}
              {st === 'failed' && (
                <p className="conn-dev-err">
                  {t(`error:${reason}`, { defaultValue: t('devices.unreachable_note') })}
                  {reason.startsWith('HTTPS_') && d.direct?.url && <span className="mono"> {d.direct.url}</span>}
                </p>
              )}
              </Fragment>
            );
          })}
        </div>
      )}
      <p className="muted small" style={{ margin: 0 }}>{t('conn.remote_note')}</p>
    </>
  );
}

/** 右上角的连接胶囊（见文件开头）。 */
export function ConnChip() {
  const { t } = useTranslation();
  const { status } = useStatus();
  const devices = useDevices();
  const focused = useFocusedDevice();
  const { open, setOpen, ref } = usePop();
  const mode = status.access === 'local' ? 'local' : 'remote';
  const activeDev = focused || devices.selected;
  const activeConn = useDeviceConn(activeDev);
  const activeOnline = !connManaged || isOnline(activeConn);
  // 远程打开时优先显示当前正在查看的会话所在电脑的连接状态（各台电脑独立）
  const paths = usePaths();
  const path = mode === 'remote' ? (activeDev ? paths[activeDev] : Object.values(paths)[0]) ?? null : null;
  const pingMs = useRemoteLatency(mode === 'remote' && activeOnline && path?.rtt_ms == null, activeDev);
  const ms = mode === 'remote' ? (activeOnline ? (path?.rtt_ms ?? pingMs) : null) : pingMs;
  const r = status.relay;
  // 没登录：这里就是开通远程访问的入口
  if (mode === 'local' && !status.account.logged_in) {
    return (
      <Link to="/account" className="conn-chip conn-off" title={t('upsell.chip_title')}>
        <Globe size={15} aria-hidden="true" />
        <span>{t('upsell.chip')}</span>
      </Link>
    );
  }
  if (mode === 'local' && (!r || r.state === 'off')) return null;
  const name = devices.multi ? deviceName(t, devices.byId(activeDev)) : status.settings?.device_name || deviceName(t, null);
  let kind = 'ok';
  let label;
  let sub = null;
  if (mode === 'local') {
    const pill = udpBlocked(status) ? 'blocked' : r.state;
    kind = pill === 'connected' ? 'ok' : pill === 'failed' ? 'bad' : pill === 'blocked' ? 'warn' : 'wait';
    label = t('conn.local');
    sub = t(`relay.pill.${pill}`);
  } else if (!activeOnline) {
    kind = activeConn.phase === 'failed' || activeConn.phase === 'offline' ? 'bad' : 'wait';
    label = t('conn.remote');
    sub = t(activeConn.phase === 'failed' ? 'devices.unreachable' : activeConn.phase === 'offline' ? 'devices.offline' : 'devices.reconnecting');
  } else {
    // 延迟和丢包率取更差的那个
    const lag = ms === null ? 'ok' : ms >= VERY_SLOW_MS ? 'bad' : ms >= SLOW_MS ? 'warn' : 'ok';
    const loss = lossLevel(path?.loss) ?? 'ok';
    kind = lag === 'bad' || loss === 'bad' ? 'bad' : lag === 'warn' || loss === 'warn' ? 'warn' : 'ok';
    label = t('conn.remote');
    // 丢包偏高时也写在胶囊上，点开看详情
    sub = [ms === null ? null : t('nav.remote_ms', { ms }), loss !== 'ok' ? t('path.loss', { loss: lossText(path.loss) }) : null].filter(Boolean).join(' · ') || null;
  }
  return (
    <span className="relay-pill-wrap" ref={ref}>
      <button
        type="button"
        className={`conn-chip conn-${kind}`}
        onClick={() => setOpen(!open)}
        aria-expanded={open}
        aria-haspopup="dialog"
        title={mode === 'remote' ? [path ? t(`path.${path.kind}`) : null, ms !== null ? t('nav.remote_latency_hint', { ms }) : null, lossText(path?.loss) ? t('path.loss', { loss: lossText(path.loss) }) : null].filter(Boolean).join(' · ') || undefined : undefined}
      >
        <span className={`dot ${path ? `path-${path.kind}` : kind === 'ok' ? 'on' : kind}`} aria-hidden="true" />
        <span>{label}</span>
        {sub && <span className="conn-sub">· {sub}</span>}
      </button>
      {open && (
        <div className="relay-pop" role="dialog" aria-label={t('conn.title')}>
          {mode === 'local' && <LocalPop />}
          {mode === 'remote' && <RemotePop ms={ms} name={name} path={path} device={path ? (activeDev ?? Object.keys(paths)[0]) : activeDev} />}
        </div>
      )}
    </span>
  );
}
