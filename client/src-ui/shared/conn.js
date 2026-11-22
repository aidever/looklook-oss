// 远程（WebRTC）打开时的连接状态：唯一来源是顶层启动页（服务端 server-web/rtc/boot.js 的 __llrtc.subscribe），
// 管理台只负责显示（docs/REMOTE_UX_2026-11-07.md §2.1、§4）。旧版启动页没有 subscribe 时退回现在的 links() 轮询（paths.jsx）。
//
// phase：boot / connecting / online / checking（切回前台确认中，不显示）/ resuming / reconnecting / failed / offline / auth
import { useEffect, useState } from 'react';

import { rtcMode } from './api.js';

const ONLINE = { phase: 'online', since: 0, lastOk: 0, hiddenFor: 0, cause: '', reason: '', device: null, others: [], dropped: 0, links: null, def: null };

let current = ONLINE;
let started = false;
const listeners = new Set();

let focusedDevice = null;
const focusListeners = new Set();

function top() {
  try {
    return window.top.__llrtc ?? null;
  } catch {
    return null;
  }
}

/** 启动页支持订阅（新版）：管理台接管状态显示。 */
export const connManaged = rtcMode && typeof top()?.subscribe === 'function';

function start() {
  if (started || !connManaged) return;
  started = true;
  const off = top().subscribe((s) => {
    current = { ...ONLINE, ...s };
    listeners.forEach((f) => f(current));
  });
  // 换页 / 刷新时取消订阅（启动页在 iframe 换页时也会清掉）
  window.addEventListener('pagehide', () => off?.());
}

/** 告诉启动页：状态由管理台显示（它藏起自己的提示条、收起骨架）。`snapshot`：有快照可以马上显示内容。 */
export function claimConn(snapshot) {
  if (!connManaged) return;
  start();
  try {
    top().claim({ snapshot: Boolean(snapshot) });
  } catch {
    /* 旧版启动页 */
  }
}

/** 当前聚焦中的电脑（当前打开会话或页面对应的电脑），供顶栏状态胶囊与连接胶囊优先跟踪。 */
export function setFocusedDevice(dev) {
  const next = dev || null;
  if (focusedDevice === next) return;
  focusedDevice = next;
  focusListeners.forEach((f) => f(focusedDevice));
}

export function useFocusedDevice() {
  const [dev, set] = useState(focusedDevice);
  useEffect(() => {
    focusListeners.add(set);
    set(focusedDevice);
    return () => focusListeners.delete(set);
  }, []);
  return dev;
}

/** 当前的连接状态（不在远程模式时永远是 online）。 */
export function connState() {
  start();
  return current;
}

export function useConn() {
  const [s, set] = useState(connState);
  useEffect(() => {
    if (!connManaged) return undefined;
    start();
    listeners.add(set);
    set(current);
    return () => listeners.delete(set);
  }, []);
  return s;
}

/** 从快照里取出某台电脑的独立连接状态；旧版启动页或 Android 壳无 `links` 时退回全局状态。 */
export function deviceConn(snapshot, devId) {
  if (!connManaged) return ONLINE;
  const s = snapshot || current;
  if (devId && s.links) {
    const l = s.links[devId];
    if (!l) {
      return { ...ONLINE, device: s.device?.id === devId ? s.device : { id: devId }, others: s.others, def: s.def };
    }
    return {
      ...ONLINE,
      ...l,
      phase: l.phase === 'idle' ? 'online' : l.phase,
      device: s.device?.id === devId ? s.device : { id: devId },
      others: s.others,
      def: s.def,
    };
  }
  return s;
}

/** 订阅某台电脑的独立连接状态（互相隔离，一台断开不影响另一台）。 */
export function useDeviceConn(devId) {
  const s = useConn();
  return deviceConn(s, devId);
}

/** 连着（远程时以启动页为准；本机、局域网永远算连着）。 */
export const isOnline = (s) => s.phase === 'online';

/** 连接确认中、恢复中、重连中：内容照常显示，只是暂时不能写。 */
export const isUnsure = (s) => ['checking', 'resuming', 'reconnecting', 'connecting', 'boot'].includes(s.phase);

/**
 * 下拉刷新、“重试”：远程时交给启动页（确认 / 重建连接后重载管理台，连不上就整页刷新）；
 * 不是远程，或者旧版启动页，就整页刷新这一层。
 */
export function refreshAll() {
  const t = top();
  if (rtcMode && typeof t?.refresh === 'function') {
    t.refresh();
    return;
  }
  window.location.reload();
}

/** 远程时让启动页对指定电脑不等退避马上重连（不重载管理台、不影响其他电脑）。 */
export function retryNow(devId) {
  const t = top();
  const dev = devId || focusedDevice || current.device?.id;
  if (dev && typeof t?.retry === 'function') return t.retry(dev);
  return refreshAll();
}

/** 远程时对单台电脑重建 WebRTC 连接（例如从“中转”重连尝试升级为局域网/公网直连），不重载页面、不影响其他电脑。 */
export function reconnectDevice(devId) {
  const t = top();
  const dev = devId || focusedDevice || current.device?.id;
  if (dev && typeof t?.reconnect === 'function') return t.reconnect(dev);
  return retryNow(dev);
}
