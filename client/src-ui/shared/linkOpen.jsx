// 对话里的网址：除了照旧在系统浏览器打开，还能交给安装了 `open_url` 能力的插件打开（协议 docs/BROWSER_PLUGIN_DESIGN_2026-11-19.md §3.1）。
//
// 交互：
// - 会话所在的电脑上没有安装声明 `open_url` 能力的插件：完全照旧，不拦点击。
// - 有插件：点链接弹一个小菜单——系统浏览器 / 各插件的目标列表（正在用的排第一）/ 复制链接，底下“记住选择”。
// - Ctrl / ⌘ / Shift / 中键点、右键、长按：都交给浏览器自己，不弹菜单。
// - 调插件 `open` 接口返回 `confirm` 时弹通用确认框，用户同意后带 `confirmed` 重发；返回 `opened` / `navigate` 时跳到对应插件页面。
import { Check, Copy, Download, ExternalLink } from 'lucide-react';
import { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { useTranslation } from 'react-i18next';

import { deviceApi } from './api.js';
import { copyText } from './chatCopy.js';
import { useDevices } from './devices.jsx';
import { errorText } from './i18n.js';
import { isDesktopApp } from './layers.js';
import { pickLocalized, pluginIcon, pluginRoute } from './plugins.jsx';
import { useRouter } from './router.jsx';
import { useConfirm, useToast } from './ui.jsx';

const PREF_KEY = 'looklook.chat-links';

/** 对话里的链接默认怎么开：ask（每次问）、system（系统浏览器）、plugin:<名>（直接交给该插件）。 */
export function linkPref() {
  try {
    const v = localStorage.getItem(PREF_KEY);
    if (v === 'system' || (v && v.startsWith('plugin:'))) return v;
    return 'ask';
  } catch {
    return 'ask';
  }
}

export function setLinkPref(v) {
  try {
    if (!v || v === 'ask') localStorage.removeItem(PREF_KEY);
    else localStorage.setItem(PREF_KEY, v);
  } catch {
    /* 存不了就每次问 */
  }
}

// 每台电脑的 open_url 能力提供者（不主动拉起未运行的插件；点开菜单时再拉取实时 targets）
const provCache = new Map();
const TTL = 30_000;

function fetchProviders(device, withTargets = false) {
  const key = `${device ?? ''}:${withTargets ? 1 : 0}`;
  const hit = provCache.get(key);
  if (hit && Date.now() - hit.at < TTL) return hit.job;
  const job = deviceApi(device)
    .get(withTargets ? '/plugins/_capabilities/open-url?fetch=true' : '/plugins/_capabilities/open-url')
    .then(
      (d) => {
        const list = Array.isArray(d?.providers) ? d.providers : [];
        provCache.set(key, { at: Date.now(), job: Promise.resolve(list), data: list });
        return list;
      },
      () => {
        provCache.delete(key);
        return [];
      },
    );
  provCache.set(key, { at: Date.now(), job, data: hit?.data ?? null });
  return job;
}

const cachedProviders = (device) => provCache.get(`${device ?? ''}:1`)?.data ?? provCache.get(`${device ?? ''}:0`)?.data ?? null;

const isWebUrl = (u) => /^https?:\/\//i.test(u);

function stripApiPrefix(p) {
  return String(p || '').replace(/^\/api(?=\/)/, '');
}

function orderedTargets(targets) {
  const list = [...(targets ?? [])];
  const rank = (x) => (x.active ? 0 : x.default ? 1 : 2);
  return list.sort((a, b) => rank(a) - rank(b));
}

export function useLinkOpen(device) {
  const { t } = useTranslation();
  const { navigate } = useRouter();
  const devices = useDevices();
  const toast = useToast();
  const confirm = useConfirm();
  const [menu, setMenu] = useState(null);

  useEffect(() => {
    fetchProviders(device, false);
  }, [device]);

  const openInPlugin = useCallback(
    async (provider, url, targetId) => {
      const client = deviceApi(device);
      const path = stripApiPrefix(provider.open_path);
      const confirmed = [];
      try {
        for (let i = 0; i < 4; i++) {
          const res = await client.post(path, { url, target: targetId ?? undefined, confirmed });
          if (!res) break;
          if (res.result === 'confirm') {
            const ok = await confirm({ title: res.title, body: res.body, confirm: res.confirm, danger: Boolean(res.danger) });
            if (!ok) return;
            confirmed.push(res.id);
            continue;
          }
          if (res.result === 'navigate' || (res.result === 'opened' && res.navigate != null)) {
            if (devices.multi && device && devices.selected !== device) devices.select(device);
            navigate(pluginRoute(provider.name, res.navigate || ''));
          }
          return;
        }
      } catch (e) {
        toast(errorText(t, e), 'error');
      }
    },
    [device, devices, navigate, confirm, toast, t],
  );

  const onClickCapture = useCallback(
    (e) => {
      if (e.defaultPrevented || e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
      if (e.target.closest?.('.md img, .link-menu')) return;
      const a = e.target.closest?.('a[href]');
      if (!a || a.hasAttribute('download') || a.classList.contains('md-file') || a.closest('[contenteditable="true"]')) return;
      const url = a.href;
      if (!isWebUrl(url)) return;
      try {
        if (new URL(url).origin === window.location.origin) return;
      } catch {
        return;
      }
      const providers = cachedProviders(device);
      if (!providers || providers.length === 0) {
        fetchProviders(device, false);
        return;
      }
      const pref = linkPref();
      if (pref === 'system') return;
      e.preventDefault();
      e.stopPropagation();
      if (pref.startsWith('plugin:')) {
        const want = pref.slice('plugin:'.length);
        const hit = providers.find((p) => p.name === want);
        if (hit) {
          openInPlugin(hit, url, null);
          return;
        }
      }
      const r = a.getBoundingClientRect();
      setMenu({ url, x: r.left, y: r.bottom + 4, top: r.top - 4, providers });
      // 点开菜单后刷新实时 targets（按需拉起插件列出全部配置）
      fetchProviders(device, true).then((fresh) => {
        if (fresh?.length) setMenu((cur) => (cur && cur.url === url ? { ...cur, providers: fresh } : cur));
      });
    },
    [device, openInPlugin],
  );

  const node = menu && (
    <LinkMenu
      {...menu}
      onClose={() => setMenu(null)}
      onPlugin={(provider, targetId, remember) => {
        if (remember) setLinkPref(`plugin:${provider.name}`);
        openInPlugin(provider, menu.url, targetId);
      }}
      onSetup={(provider, setupPath) => {
        if (devices.multi && device && devices.selected !== device) devices.select(device);
        const q = `?open=${encodeURIComponent(menu.url)}`;
        navigate(`${pluginRoute(provider.name, setupPath || '')}${q}`);
      }}
      onCopy={async () => {
        if (await copyText(menu.url)) toast(t('chat_link.copied'));
        else window.prompt('', menu.url);
      }}
    />
  );
  return { onClickCapture, menu: node };
}

function LinkMenu({ url, x, y, top, providers, onClose, onPlugin, onSetup, onCopy }) {
  const { t, i18n } = useTranslation();
  const ref = useRef(null);
  const [remember, setRemember] = useState(false);
  const [pos, setPos] = useState({ left: x, top: y, visibility: 'hidden' });

  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const w = el.offsetWidth;
    const h = el.offsetHeight;
    const below = y + h + 8 <= window.innerHeight;
    setPos({ left: Math.max(8, Math.min(x, window.innerWidth - w - 8)), top: Math.max(8, below ? y : top - h) });
  }, [x, y, top, providers]);

  useEffect(() => {
    const away = (e) => !ref.current?.contains(e.target) && onClose();
    const key = (e) => e.key === 'Escape' && onClose();
    document.addEventListener('pointerdown', away, true);
    document.addEventListener('keydown', key);
    window.addEventListener('resize', onClose);
    window.addEventListener('scroll', onClose, true);
    return () => {
      document.removeEventListener('pointerdown', away, true);
      document.removeEventListener('keydown', key);
      window.removeEventListener('resize', onClose);
      window.removeEventListener('scroll', onClose, true);
    };
  }, [onClose]);

  useEffect(() => {
    ref.current?.querySelector('a, button')?.focus({ preventScroll: true });
  }, []);

  let host = url;
  try {
    host = new URL(url).host;
  } catch {
    /* 照原样显示 */
  }
  const pick = (fn) => () => {
    onClose();
    fn();
  };

  return createPortal(
    <ul ref={ref} className="menu-pop-list menu-pop-fixed link-menu" role="menu" style={pos} aria-label={url}>
      <li className="menu-pop-head link-menu-url" role="presentation" title={url}>
        {host}
      </li>
      <li>
        <a
          role="menuitem"
          className="link-menu-item"
          href={url}
          target="_blank"
          rel="noopener noreferrer"
          onClick={() => {
            if (remember) setLinkPref('system');
            onClose();
          }}
        >
          <ExternalLink size={17} aria-hidden="true" />
          <span className="grow">{t(isDesktopApp ? 'chat_link.system' : 'chat_link.system_tab')}</span>
        </a>
      </li>
      {providers.map((p) => {
        const Icon = pluginIcon(p.icon);
        const title = pickLocalized(p.title, i18n.language) || p.name;
        const d = p.data;
        if (d && d.ready === false) {
          return (
            <li key={p.name}>
              <button type="button" role="menuitem" onClick={pick(() => onSetup(p, d.setup_path))}>
                <Download size={17} aria-hidden="true" />
                <span className="grow">
                  {title}
                  {d.setup_hint && <span className="menu-pop-hint">{d.setup_hint}</span>}
                </span>
              </button>
            </li>
          );
        }
        const targets = orderedTargets(d?.targets).slice(0, 5);
        if (targets.length === 0) {
          return (
            <li key={p.name}>
              <button type="button" role="menuitem" onClick={pick(() => onPlugin(p, null, remember))}>
                <Icon size={17} aria-hidden="true" />
                <span className="grow">{t('chat_link.open_with', { name: title })}</span>
              </button>
            </li>
          );
        }
        return targets.map((tg) => (
          <li key={`${p.name}:${tg.id}`}>
            <button type="button" role="menuitem" onClick={pick(() => onPlugin(p, tg.id, remember))}>
              <Icon size={17} aria-hidden="true" />
              <span className="grow">
                {targets.length > 1 ? `${title} · ${tg.label}` : t('chat_link.open_with', { name: title })}
                {tg.hint && <span className="menu-pop-hint">{tg.hint}</span>}
              </span>
            </button>
          </li>
        ));
      })}
      <li>
        <button type="button" role="menuitem" onClick={pick(onCopy)}>
          <Copy size={17} aria-hidden="true" />
          <span className="grow">{t('chat_link.copy')}</span>
        </button>
      </li>
      <li className="menu-pop-sep" role="separator" />
      <li>
        <button type="button" role="menuitemcheckbox" aria-checked={remember} className="link-menu-remember" onClick={() => setRemember((v) => !v)}>
          <Check size={15} aria-hidden="true" style={{ visibility: remember ? 'visible' : 'hidden' }} />
          <span className="grow">
            {t('chat_link.remember')}
            <span className="menu-pop-hint">{t('chat_link.remember_hint')}</span>
          </span>
        </button>
      </li>
    </ul>,
    document.body,
  );
}
