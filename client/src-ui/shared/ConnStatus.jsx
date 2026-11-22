// 远程打开时唯一的连接状态提示（docs/REMOTE_UX_2026-11-07.md §4、§5）：顶上居中一个小胶囊，不挡内容、不清空页面。
// 状态来自启动页（conn.js），这里只负责按时长逐级说清楚：抖一下什么都不显示，久了说原因，再久给“重试”。
// 连回来时断开超过 3 秒才轻提示“已重新连接”；确认连接期间没发出去的终端按键给个提示（停得久、可关）。
import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';

import { clearSnapshot } from './snapshot.js';
import { connManaged, deviceConn, retryNow, useConn, useFocusedDevice } from './conn.js';
import { deviceName, useDevices } from './devices.jsx';

/** 断开（重连中）多久以后才显示：心跳确认、并行建连一般 2 秒内就好了。 */
const SHOW_DROP_AFTER = 2000;
/** 第一次连接（按快照显示着内容）多久以后才显示“正在连接”。 */
const SHOW_CONNECT_AFTER = 1000;
/** 多久以后换成“暂时连不上”并给“重试”。 */
const SLOW_AFTER = 15000;
/** 断开超过这么久，连回来时提示一下。 */
const TOAST_AFTER = 3000;

function useNow(active) {
  const [now, setNow] = useState(Date.now);
  useEffect(() => {
    if (!active) return undefined;
    const id = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(id);
  }, [active]);
  return now;
}

export default function ConnStatus() {
  const { t } = useTranslation();
  const devices = useDevices();
  const focused = useFocusedDevice();
  const allConn = useConn();
  const targetDev = focused || devices.selected || allConn.device?.id || null;
  const s = deviceConn(allConn, targetDev);
  const busy = ['connecting', 'boot', 'resuming', 'reconnecting'].includes(s.phase);
  const now = useNow(busy);
  // 轻提示分两档：info（已重新连接）2 秒自己走；warn（按键没发出去）要用户看到，停 10 秒、文字完整折行、带 ×。
  // 点提示任意处都能关。计时只跟着提示本身走——之前跟着连接状态 s 走，心跳一更新 s 就把计时清掉了，提示一直不消失。
  const [toast, setToast] = useState(null);
  const prev = useRef({ dev: targetDev, s });
  const devObj = targetDev ? devices.byId(targetDev) : null;
  const name = (devObj ? deviceName(t, devObj) : null) || s.device?.name;
  useEffect(() => {
    const p = prev.current;
    if (p.dev !== targetDev) {
      prev.current = { dev: targetDev, s };
      return;
    }
    const ps = p.s;
    prev.current = { dev: targetDev, s };
    if (allConn.phase === 'auth') clearSnapshot();
    if (s.dropped > ps.dropped) {
      const n = s.dropped - ps.dropped;
      setToast((old) => ({ level: 'warn', n: n + (old?.level === 'warn' ? old.n : 0), id: Date.now() }));
    } else if (s.phase === 'online' && ps.phase !== 'online' && ['resuming', 'reconnecting', 'failed', 'offline'].includes(ps.phase) && Date.now() - ps.since >= TOAST_AFTER) {
      // 已重新连接不盖掉还没看完的“按键没发出”
      setToast((old) => (old?.level === 'warn' ? old : { level: 'info', name: devices.multi ? name : null, id: Date.now() }));
    }
  }, [allConn.phase, targetDev, s, devices.multi, name]);
  useEffect(() => {
    if (!toast) return undefined;
    const id = setTimeout(() => setToast(null), toast.level === 'warn' ? 10000 : 2000);
    return () => clearTimeout(id);
  }, [toast]);
  if (!connManaged) return null;

  const age = now - (s.since || now);
  const labelName = devices.multi ? name : null;
  let text = '';
  let slow = false;
  if (s.phase === 'resuming') text = labelName ? t('rlink.resuming_name', { name: labelName }) : t('rlink.resuming');
  else if (s.phase === 'offline') {
    text = t('rlink.device_offline', { name: name || t('rlink.computer') });
  } else if (s.phase === 'failed') {
    slow = true;
    text = s.cause === 'net' ? t('rlink.net') : labelName ? t('rlink.failed_name', { name: labelName }) : t('rlink.failed');
  } else if (s.phase === 'reconnecting' && age >= SHOW_DROP_AFTER) {
    slow = age >= SLOW_AFTER;
    const secs = Math.round(age / 1000);
    if (s.cause === 'net') text = t('rlink.net');
    else if (s.cause === 'device_offline') text = t('rlink.device_offline', { name: name || t('rlink.computer') });
    else if (slow) text = labelName ? t('rlink.slow_name', { name: labelName, s: secs }) : t('rlink.slow', { s: secs });
    else text = labelName ? t('rlink.reconnecting_name', { name: labelName, s: secs }) : t('rlink.reconnecting', { s: secs });
  } else if ((s.phase === 'connecting' || s.phase === 'boot') && age >= SHOW_CONNECT_AFTER) {
    slow = age >= SLOW_AFTER / 2;
    text = name ? t('rlink.connecting_name', { name }) : t('rlink.connecting');
  }
  if (!text && !toast) return null;
  const warn = !text && toast?.level === 'warn';
  return (
    <div
      className={`conn-pill ${text ? '' : 'conn-toast'} ${warn ? 'warn' : ''} ${slow ? 'slow' : ''}`}
      role={warn ? 'alert' : 'status'}
      aria-live={warn ? 'assertive' : 'polite'}
      onClick={text ? undefined : () => setToast(null)}
    >
      {text ? (
        <>
          <span className="conn-dots" aria-hidden="true">
            <i />
            <i />
            <i />
          </span>
          <span className="conn-text">{text}</span>
          {slow && (
            <button type="button" onClick={() => retryNow(targetDev)}>
              {t('rlink.retry')}
            </button>
          )}
        </>
      ) : (
        <>
          <span className="conn-text">{warn ? t('rlink.dropped', { count: toast.n }) : toast.name ? t('rlink.back_name', { name: toast.name }) : t('rlink.back')}</span>
          {warn && (
            <button type="button" className="conn-x" aria-label={t('action.close')} onClick={() => setToast(null)}>
              ×
            </button>
          )}
        </>
      )}
    </div>
  );
}
