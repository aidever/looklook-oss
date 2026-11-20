// 对话里的网址（回复里的链接、工具结果里的地址……）除了照旧在系统浏览器（新标签页）打开，还能交给会话所在电脑上
// 能打开网址的插件（清单里 `open_url: true`，例如远程浏览器）。看看不认识具体插件：菜单里每个这样的插件一行。
//
// - 会话所在的电脑上没有这样的插件：完全照旧，不拦点击。
// - 有：点链接弹一个小菜单——系统浏览器 / 用 <插件> 打开 / 复制链接，底下“记住选择”（记在这个浏览器里）。
// - Ctrl / ⌘ / Shift / 中键点、右键、长按：都交给浏览器自己，不弹菜单。
// - 交给插件：`POST /api/plugins/<名>/open-url { url }`（插件自己决定在哪个窗口、哪套配置里开），然后跳到插件页。
import { Check, Copy, ExternalLink } from 'lucide-react';
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
import { useToast } from './ui.jsx';

const PREF_KEY = 'looklook.chat-links';

/** 对话里的链接默认怎么开：`ask`（每次问）、`system`（系统浏览器）、`plugin:<名>`（交给这个插件） */
export function linkPref() {
  try {
    const v = localStorage.getItem(PREF_KEY);
    return v === 'system' || /^plugin:[a-z][a-z0-9-]{1,31}$/.test(v ?? '') ? v : 'ask';
  } catch {
    return 'ask';
  }
}

export function setLinkPref(v) {
  try {
    if (v === 'ask') localStorage.removeItem(PREF_KEY);
    else localStorage.setItem(PREF_KEY, v);
  } catch {
    /* 存不了就每次问 */
  }
}

// 每台电脑上能打开网址的插件（`GET /status` 里的 plugins），点链接时不等网络
const cache = new Map();
const TTL = 30_000;
function fetchTargets(device) {
  const key = device ?? '';
  const hit = cache.get(key);
  if (hit && Date.now() - hit.at < TTL) return;
  cache.set(key, { at: Date.now(), data: hit?.data ?? null });
  deviceApi(device)
    .get('/status')
    .then(
      (s) => cache.set(key, { at: Date.now(), data: (s?.plugins ?? []).filter((p) => p.open_url && !p.incompatible) }),
      () => cache.delete(key),
    );
}
const cachedTargets = (device) => cache.get(device ?? '')?.data ?? [];

const isWebUrl = (u) => /^https?:\/\//i.test(u);

/**
 * 对话页用：`device` 是会话所在的电脑（多台电脑时；一台时为 null）。
 * 返回 { onClickCapture, menu }：onClickCapture 挂在对话的最外层（捕获阶段，各处的 <a> 都不用改），menu 放进页面里。
 */
export function useLinkOpen(device) {
  const { t } = useTranslation();
  const { navigate } = useRouter();
  const devices = useDevices();
  const toast = useToast();
  const [menu, setMenu] = useState(null);

  useEffect(() => {
    fetchTargets(device);
  }, [device]);

  const open = useCallback(
    async (url, name) => {
      try {
        await deviceApi(device).post(`/plugins/${name}/open-url`, { url });
        if (devices.multi && device && devices.selected !== device) devices.select(device);
        navigate(pluginRoute(name));
      } catch (e) {
        toast(errorText(t, e), 'error');
      }
    },
    [device, devices, navigate, toast, t],
  );

  const onClickCapture = useCallback(
    (e) => {
      if (e.defaultPrevented || e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
      // 链接里的图片：点了是看大图（usePreviewLinks）；菜单自己（portal 里的点击照样冒泡到对话上）
      if (e.target.closest?.('.md img, .link-menu')) return;
      const a = e.target.closest?.('a[href]');
      if (!a || a.hasAttribute('download') || a.classList.contains('md-file') || a.closest('[contenteditable="true"]')) return;
      const url = a.href;
      if (!isWebUrl(url)) return;
      try {
        // 管理台自己的地址（文件、下载……）不算
        if (new URL(url).origin === window.location.origin) return;
      } catch {
        return;
      }
      const targets = cachedTargets(device);
      fetchTargets(device);
      if (targets.length === 0) return;
      const pref = linkPref();
      if (pref === 'system') return;
      e.preventDefault();
      e.stopPropagation();
      const remembered = pref.startsWith('plugin:') && targets.find((p) => `plugin:${p.name}` === pref);
      if (remembered) {
        open(url, remembered.name);
        return;
      }
      const r = a.getBoundingClientRect();
      setMenu({ url, x: r.left, y: r.bottom + 4, top: r.top - 4, targets });
    },
    [device, open],
  );

  const node = menu && (
    <LinkMenu
      {...menu}
      onClose={() => setMenu(null)}
      onPlugin={(name, remember) => {
        if (remember) setLinkPref(`plugin:${name}`);
        open(menu.url, name);
      }}
      onCopy={async () => {
        if (await copyText(menu.url)) toast(t('chat_link.copied'));
        else window.prompt('', menu.url);
      }}
    />
  );
  return { onClickCapture, menu: node };
}

function LinkMenu({ url, x, y, top, targets, onClose, onPlugin, onCopy }) {
  const { t, i18n } = useTranslation();
  const ref = useRef(null);
  const [remember, setRemember] = useState(false);
  const [pos, setPos] = useState({ left: x, top: y, visibility: 'hidden' });
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const w = el.offsetWidth;
    const h = el.offsetHeight;
    // 下面放不下就放到链接上面
    const below = y + h + 8 <= window.innerHeight;
    setPos({ left: Math.max(8, Math.min(x, window.innerWidth - w - 8)), top: Math.max(8, below ? y : top - h) });
  }, [x, y, top]);
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
        {/* 真的链接：和原来一样由浏览器（桌面版是系统浏览器）打开，不经 window.open，不会被拦 */}
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
      {targets.map((p) => {
        const Icon = pluginIcon(p.icon);
        return (
          <li key={p.name}>
            <button type="button" role="menuitem" onClick={pick(() => onPlugin(p.name, remember))}>
              <Icon size={17} aria-hidden="true" />
              <span className="grow">
                {t('chat_link.open_in', { name: pickLocalized(p.title, i18n.language) || p.name })}
                {p.running && <span className="menu-pop-hint">{t('plugins.running')}</span>}
              </span>
            </button>
          </li>
        );
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
