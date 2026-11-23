// 看看开着时的提醒（docs/TASK_BOARD.md §6、规则 fg_quiet）：页面在前台时每 20 秒告诉各台电脑一声，电脑就先不发飞书和其它渠道的通知
// （规则 fg_quiet，notify.rs），改成这里弹提醒：别的会话在等你处理、做完了、出错了。正开着的那个会话不提醒。
import { BellRing, CheckCircle2, X, XCircle } from 'lucide-react';
import { useEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';

import { deviceApi } from './api.js';
import { boardState } from './board.js';
import { deviceName, useDevices } from './devices.jsx';
import { linkState, useLinks } from './paths.jsx';
import { petNotify, useDeskNotify } from './ChatPet.jsx';
import { useSessions } from './sessions.jsx';

const PING_MS = 20000;
// 跟电脑端“做完了”通知一样：太短的一轮不提醒
const DONE_MIN_MS = 30000;
// 每条提醒留多久（鼠标停在卡片上时不走）
const KEEP_MS = { done: 10000, failed: 20000, attn: 20000 };
// 卡片里最多列几条，再多的收成“还有 N 条”
const SHOW_MAX = 3;

const foreground = () => !document.hidden && document.hasFocus();

/** 页面在前台时告诉列表里的每台电脑（旧版电脑没有这个接口：出错就不再报） */
export function useForegroundPing() {
  const sessions = useSessions();
  const devs = useMemo(() => [...new Set((sessions?.items ?? []).filter((i) => !i.offline).map((i) => i.device ?? ''))].sort().join('\n'), [sessions?.items]);
  const dead = useRef(new Set());
  useEffect(() => {
    const list = devs ? devs.split('\n') : [''];
    const ping = () => {
      if (!foreground()) return;
      for (const d of list) {
        if (dead.current.has(d)) continue;
        deviceApi(d || null)
          .post('/notify/foreground')
          .catch((e) => {
            if (e?.status === 404 || e?.status === 405) dead.current.add(d);
          });
      }
    };
    ping();
    const timer = setInterval(ping, PING_MS);
    window.addEventListener('focus', ping);
    document.addEventListener('visibilitychange', ping);
    return () => {
      clearInterval(timer);
      window.removeEventListener('focus', ping);
      document.removeEventListener('visibilitychange', ping);
    };
  }, [devs]);
}

/**
 * 别的会话在等你处理、这一轮做完 / 出错了：右上角（手机上在顶栏下面）一张卡片，一个会话一行，点一行打开那个会话。
 * `activeId`：正开着的会话，不提醒，打开了的会话的那一行也拿掉。
 *
 * 不重复提醒以前的事：
 * - 只有上一次看到它还在跑、这次跑完了才算“做完了 / 出错了”（会话的状态暂时没取到、电脑掉线又回来、客户端重启后
 *   重新给出上一轮，都不会再弹）；
 * - 记下每个会话见过的最后一轮的结束时间，只增不减，会话暂时从列表里消失也不忘；
 * - 同一轮、同一次“等你处理”只提醒一次。
 */
// 提到模块级：手机上进终端页（/t/）会卸掉管理台再重新挂载，状态放组件里返回后会清空（漏提醒 / 重复提醒）
const seen = { current: null };
const told = { current: new Set() };

export function InAppAlerts({ activeId, open }) {
  const { t } = useTranslation();
  const sessions = useSessions();
  const [list, setList] = useState([]);
  const [hover, setHover] = useState(false);
  const [expanded, setExpanded] = useState(false);
  const openRef = useRef(open);
  openRef.current = open;
  // 开着宠物、开了“宠物来报信”：交给宠物说（页面里的或桌面宠物），不弹卡片
  useDeskNotify((id) => openRef.current?.(id));

  useEffect(() => {
    const items = sessions?.items;
    if (!items) return;
    const prev = seen.current;
    const next = new Map(prev ?? []);
    const add = [];
    for (const i of items) {
      const c = i.chat ?? {};
      const state = boardState(i);
      const attn = state === 'attn' ? `${c.since ?? ''}|${c.pending?.kind ?? c.phase ?? ''}` : '';
      const turn = c.turn ?? null;
      const end = turn?.ended_at ?? 0;
      const busy = state === 'working' || state === 'attn' || Boolean(turn && turn.ended_at == null && turn.started_at);
      const p = prev?.get(i.id);
      next.set(i.id, { attn, end: Math.max(end, p?.end ?? 0), busy });
      if (!p || i.id === activeId || !foreground()) continue;
      let kind = null;
      let key = '';
      if (attn && attn !== p.attn) {
        kind = 'attn';
        key = `${i.id}|a|${attn}`;
      } else if (end > p.end && p.busy && turn?.outcome === 'failed') {
        kind = 'failed';
        key = `${i.id}|e|${end}`;
      } else if (end > p.end && p.busy && turn?.outcome === 'done' && end - (turn.started_at ?? end) >= DONE_MIN_MS) {
        kind = 'done';
        key = `${i.id}|e|${end}`;
      }
      if (!kind || told.current.has(key)) continue;
      told.current.add(key);
      const title = c.title || i.name || (i.workdir ?? '').split(/[\\/]/).filter(Boolean).pop() || '';
      add.push({ id: i.id, kind, title, at: Date.now() });
    }
    seen.current = next;
    if (told.current.size > 500) told.current = new Set([...told.current].slice(-200));
    // 宠物说了的不再弹卡片
    const rest = add.filter((a) => !petNotify(a, (id) => openRef.current?.(id)));
    add.length = 0;
    add.push(...rest);
    // 同一个会话只留最新的一条
    if (add.length) setList((xs) => [...add, ...xs.filter((x) => !add.some((a) => a.id === x.id))]);
  }, [sessions?.items]);

  // 打开了的会话：它那一行不用再留着
  useEffect(() => {
    if (activeId) setList((xs) => (xs.some((x) => x.id === activeId) ? xs.filter((x) => x.id !== activeId) : xs));
  }, [activeId]);

  // 到时间自己消失；鼠标停在卡片上时不走，移开后重新计时
  useEffect(() => {
    if (!list.length || hover) return;
    const now = Date.now();
    const due = Math.min(...list.map((x) => x.at + KEEP_MS[x.kind]));
    const timer = setTimeout(() => setList((xs) => xs.filter((x) => x.at + KEEP_MS[x.kind] > Date.now())), Math.max(0, due - now) + 50);
    return () => clearTimeout(timer);
  }, [list, hover]);
  useEffect(() => {
    if (!list.length) setExpanded(false);
  }, [list.length]);

  // 手机上往上或往旁边划走整张卡片；划过之后松手那一下不算点了某一行
  const swipe = useRef({ start: null, moved: false });
  const onDown = (e) => {
    swipe.current = { start: { x: e.clientX, y: e.clientY }, moved: false };
  };
  const onUp = (e) => {
    const st = swipe.current.start;
    if (st && (st.y - e.clientY > 40 || Math.abs(e.clientX - st.x) > 40)) {
      swipe.current.moved = true;
      setList([]);
    }
    swipe.current.start = null;
  };

  if (!list.length) return null;
  const drop = (id) => setList((xs) => xs.filter((x) => x.id !== id));
  const clear = () => setList([]);
  const leave = () => {
    setHover(false);
    setList((xs) => xs.map((x) => ({ ...x, at: Date.now() })));
  };
  const shown = expanded ? list : list.slice(0, SHOW_MAX);
  const more = list.length - shown.length;
  const Icon = { done: CheckCircle2, failed: XCircle, attn: BellRing };
  return (
    <>
      <div className="notice-card" onMouseEnter={() => setHover(true)} onMouseLeave={leave} onPointerDown={onDown} onPointerUp={onUp}>
        <div className={`notice-rows ${expanded ? 'notice-rows-all' : ''}`}>
          {shown.map((x) => {
            const I = Icon[x.kind];
            return (
              <div
                key={x.id}
                className={`notice-row notice-${x.kind}`}
                role="button"
                tabIndex={0}
                onClick={() => {
                  if (swipe.current.moved) return;
                  drop(x.id);
                  open?.(x.id);
                }}
                onKeyDown={(e) => {
                  if (e.key === 'Enter') {
                    drop(x.id);
                    open?.(x.id);
                  }
                }}
                title={t('inapp.open')}
              >
                <I size={18} className="notice-icon" />
                <span className="notice-text">
                  <span className="notice-title">{x.title || t('inapp.untitled')}</span>
                  <span className="notice-state">{t(`inapp.${x.kind}`)}</span>
                </span>
                <button
                  type="button"
                  className="notice-x"
                  aria-label={t('action.close')}
                  onClick={(e) => {
                    e.stopPropagation();
                    drop(x.id);
                  }}
                >
                  <X size={15} />
                </button>
              </div>
            );
          })}
        </div>
        {/* 多条时底下一行：还有几条（点开看全部）、全部关掉 */}
        {list.length > 1 && (
          <div className="notice-foot">
            {more > 0 ? (
              <button type="button" className="notice-link" onClick={() => setExpanded(true)}>
                {t('inapp.more', { count: more })}
              </button>
            ) : (
              <span />
            )}
            <button type="button" className="notice-link" onClick={clear}>
              {t('inapp.clear')}
            </button>
          </div>
        )}
      </div>
    </>
  );
}

/**
 * 连不上某台电脑（连接状态 failed，比如公网 HTTPS 直连不通）：右上角一张卡片写明原因，不自动消失，要点叉关掉。
 * 关掉的只在这次失败里不再弹；那台电脑连上过（不再是 failed）、或者原因变了，再失败时重新弹。
 */
export function ConnFailAlerts() {
  const { t } = useTranslation();
  const devices = useDevices();
  const links = useLinks();
  const [closed, setClosed] = useState(() => new Set());
  const failed = (devices?.items ?? []).filter((d) => d.online && linkState(d, links) === 'failed');
  const key = (d) => `${d.id}|${links[d.id]?.reason || ''}`;
  const live = new Set(failed.map(key));
  // 恢复了或者原因变了：忘掉关过的，下次失败再弹
  useEffect(() => {
    setClosed((xs) => {
      const keep = new Set([...xs].filter((k) => live.has(k)));
      return keep.size === xs.size ? xs : keep;
    });
  }, [[...live].sort().join('\n')]);
  const shown = failed.filter((d) => !closed.has(key(d)));
  if (!shown.length) return null;
  return (
    <div className="notice-card" role="alert">
      <div className="notice-rows">
        {shown.map((d) => {
          const reason = links[d.id]?.reason || '';
          const addr = reason.startsWith('HTTPS_') ? d.direct?.url : null;
          return (
            <div key={d.id} className="notice-row notice-failed notice-conn">
              <XCircle size={18} className="notice-icon" />
              <span className="notice-text notice-text-col">
                <span className="notice-title">{t('inapp.conn_failed', { name: deviceName(t, d) })}</span>
                <span className="notice-detail">
                  {t(`error:${reason}`, { defaultValue: t('devices.unreachable_note') })}
                  {addr && <span className="mono"> {addr}</span>}
                </span>
              </span>
              <button type="button" className="notice-x" aria-label={t('action.close')} onClick={() => setClosed((xs) => new Set(xs).add(key(d)))}>
                <X size={15} />
              </button>
            </div>
          );
        })}
      </div>
    </div>
  );
}
