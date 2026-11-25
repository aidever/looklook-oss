// 宠物版本号、桌面宠物状态的推送（gateway/live_ws.rs）：整页共用。
// - 本机明文 http 打开（桌面应用、本机浏览器、局域网）：一条 WebSocket（/api/live/ws）。以前是两条长轮询（每条挂最多 25 秒），
//   桌面应用主窗口 + 每块屏幕的桌面宠物窗口加起来能把浏览器同一地址的 6 条 HTTP 连接占满，别的请求（看文件、发图片、新建会话）
//   要排队等长轮询回来。WebSocket 不占这 6 条。
// - 远程打开（https 的 HTTP/2、WebRTC 经 Service Worker）：没有这个限制，照旧长轮询。
// - WebSocket 一直连不上（一次都没连上、连续失败几次）：退回长轮询。

import { api, localApi, rtcMode } from './api.js';

const RETRY = [1000, 2000, 5000, 10000];
const GIVE_UP = 3;

const subs = { pets: new Set(), desk: new Set() };
const last = { pets: null, desk: null };
let mode = (() => {
  try {
    return location.protocol === 'http:' && !rtcMode && typeof WebSocket === 'function' ? 'ws' : 'poll';
  } catch {
    return 'poll';
  }
})();

function emit(t, v) {
  last[t] = v;
  for (const fn of subs[t]) fn(v);
}

const any = () => subs.pets.size > 0 || subs.desk.size > 0;

// ---------------- WebSocket ----------------

let ws = null;
let everOpened = false;
let failures = 0;
let retryTimer = null;

function connect() {
  clearTimeout(retryTimer);
  retryTimer = null;
  if (ws || !any()) return;
  const sock = new WebSocket(`ws://${location.host}/api/live/ws`);
  ws = sock;
  sock.onopen = () => {
    everOpened = true;
    failures = 0;
  };
  sock.onmessage = (e) => {
    let m;
    try {
      m = JSON.parse(e.data);
    } catch {
      return;
    }
    if (m?.t === 'pets' && typeof m.ver === 'number') emit('pets', m.ver);
    else if (m?.t === 'desk' && m.data) emit('desk', m.data);
  };
  sock.onclose = () => {
    if (ws === sock) ws = null;
    if (!any()) return;
    failures += 1;
    if (!everOpened && failures >= GIVE_UP) {
      mode = 'poll';
      startPolls();
      return;
    }
    retryTimer = setTimeout(connect, RETRY[Math.min(failures - 1, RETRY.length - 1)]);
  };
}

function maybeClose() {
  if (any()) return;
  clearTimeout(retryTimer);
  retryTimer = null;
  const sock = ws;
  ws = null;
  sock?.close();
}

// ---------------- 长轮询（远程打开、WebSocket 用不了） ----------------

const polling = { pets: false, desk: false };

async function pollLoop(t) {
  if (polling[t]) return;
  polling[t] = true;
  try {
    while (subs[t].size) {
      try {
        if (t === 'pets') {
          const cur = last.pets;
          const r = await (rtcMode ? api : localApi).get(`/pets/version${cur == null ? '' : `?since=${cur}`}`);
          if (typeof r?.ver === 'number') emit('pets', r.ver);
          await new Promise((ok) => setTimeout(ok, 200));
        } else {
          const cur = last.desk?.seq;
          const r = await localApi.get(`/desktop/pet${cur == null ? '' : `?since=${cur}`}`);
          if (r) emit('desk', r);
          await new Promise((ok) => setTimeout(ok, 50));
        }
      } catch {
        // 连不上（客户端旧、断网）：过一会儿再试
        await new Promise((ok) => setTimeout(ok, t === 'pets' ? 10000 : 3000));
      }
    }
  } finally {
    polling[t] = false;
  }
}

function startPolls() {
  for (const t of ['pets', 'desk']) if (subs[t].size) pollLoop(t);
}

// ---------------- 订阅 ----------------

function subscribe(t, fn) {
  subs[t].add(fn);
  if (last[t] != null) fn(last[t]);
  if (mode === 'ws') connect();
  else pollLoop(t);
  return () => {
    subs[t].delete(fn);
    if (mode === 'ws') maybeClose();
  };
}

/** 宠物版本号（变了就该重新取设置、宠物、台词）：`fn(ver)`，订阅时有就先给一次。返回退订函数 */
export const onPetsVer = (fn) => subscribe('pets', fn);

/** 桌面宠物状态（同 GET /api/desktop/pet）：`fn(state)`，订阅时有就先给一次。返回退订函数 */
export const onDeskState = (fn) => subscribe('desk', fn);
