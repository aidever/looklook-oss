// 通用插件页面容器（/plugin/<名>[/<路径>]）：同源 iframe 嵌入插件自带界面，
// 外壳统一负责设备切换、顶栏、全屏与 postMessage 桥（主题、语言、当前设备、连接状态、标题、导航、提示）。
import { Maximize2, Minimize2, Puzzle, Settings2 } from 'lucide-react';
import { useEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';

import { ConnChip } from '../shared/RelayStatus.jsx';
import { DeviceTag } from '../shared/brands.jsx';
import { DeviceSwitcher, useDevices } from '../shared/devices.jsx';
import { UI_MESSAGE_TAG, pickLocalized, pluginIcon, pluginIframeSrc } from '../shared/plugins.jsx';
import { useRouter } from '../shared/router.jsx';
import { useStatus } from '../App.jsx';
import { Button, Empty, Note, useToast } from '../shared/ui.jsx';

function currentTheme() {
  return document.documentElement.dataset.theme || (window.matchMedia?.('(prefers-color-scheme: dark)').matches ? 'dark' : 'light');
}

function connSnapshot() {
  try {
    const rtc = window.top?.__llrtc;
    return rtc && typeof rtc.state === 'function' ? rtc.state() : { phase: 'local' };
  } catch {
    return { phase: 'local' };
  }
}

export default function PluginView({ name, entryPath = '' }) {
  const { t, i18n } = useTranslation();
  const { status } = useStatus();
  const devices = useDevices();
  const { navigate } = useRouter();
  const toast = useToast();

  const wrapRef = useRef(null);
  const frameRef = useRef(null);
  const [customTitle, setCustomTitle] = useState(null);
  const [isFull, setIsFull] = useState(false);

  const plugin = useMemo(() => (status?.plugins ?? []).find((p) => p.name === name) ?? null, [status?.plugins, name]);
  const device = devices.byId(devices.selected);
  const deviceId = devices.multi ? devices.selected : null;

  useEffect(() => {
    setCustomTitle(null);
  }, [name, entryPath, deviceId]);

  const post = (msg) => {
    const win = frameRef.current?.contentWindow;
    if (!win) return;
    try {
      win.postMessage({ ll: UI_MESSAGE_TAG, v: 1, ...msg }, window.location.origin);
    } catch {
      /* iframe 尚未就绪 */
    }
  };

  const sendInit = () => {
    post({
      type: 'init',
      theme: currentTheme(),
      locale: i18n.language,
      device: deviceId,
      conn: connSnapshot(),
    });
  };

  // 监听插件发来的 postMessage
  useEffect(() => {
    const onMessage = (ev) => {
      if (ev.origin !== window.location.origin) return;
      if (ev.source !== frameRef.current?.contentWindow) return;
      const d = ev.data;
      if (!d || d.ll !== UI_MESSAGE_TAG || d.v !== 1) return;
      if (d.type === 'ready') {
        sendInit();
      } else if (d.type === 'title' && typeof d.text === 'string') {
        setCustomTitle(d.text.slice(0, 80));
      } else if (d.type === 'fullscreen') {
        const el = wrapRef.current;
        if (!el) return;
        if (d.on && !document.fullscreenElement) el.requestFullscreen?.().catch(() => {});
        else if (!d.on && document.fullscreenElement) document.exitFullscreen?.().catch(() => {});
      } else if (d.type === 'navigate' && typeof d.to === 'string' && d.to.startsWith('/') && !d.to.startsWith('//')) {
        navigate(d.to);
      } else if (d.type === 'toast' && typeof d.text === 'string') {
        toast(d.text, d.kind === 'error' ? 'error' : undefined);
      }
    };
    window.addEventListener('message', onMessage);
    return () => window.removeEventListener('message', onMessage);
  }, [i18n.language, deviceId, navigate, toast]);

  // 同步语言、设备变化
  useEffect(() => {
    post({ type: 'locale', locale: i18n.language });
  }, [i18n.language]);

  useEffect(() => {
    post({ type: 'device', device: deviceId });
  }, [deviceId]);

  // 同步主题变化
  useEffect(() => {
    const ob = new MutationObserver(() => post({ type: 'theme', theme: currentTheme() }));
    ob.observe(document.documentElement, { attributes: true, attributeFilter: ['data-theme', 'class'] });
    return () => ob.disconnect();
  }, []);

  // 同步远程 WebRTC 连接状态（供插件全屏浮窗显示）
  useEffect(() => {
    let unsub = null;
    try {
      const rtc = window.top?.__llrtc;
      if (rtc && typeof rtc.subscribe === 'function') {
        unsub = rtc.subscribe((st) => post({ type: 'conn', conn: st }));
      }
    } catch {
      /* 忽略 */
    }
    return () => unsub?.();
  }, []);

  useEffect(() => {
    const onFs = () => setIsFull(document.fullscreenElement === wrapRef.current);
    document.addEventListener('fullscreenchange', onFs);
    return () => document.removeEventListener('fullscreenchange', onFs);
  }, []);

  const toggleFull = () => {
    const el = wrapRef.current;
    if (!el) return;
    if (document.fullscreenElement) document.exitFullscreen?.().catch(() => {});
    else el.requestFullscreen?.().catch(() => {});
  };

  const Icon = pluginIcon(plugin?.icon);
  const title = customTitle || (plugin ? pickLocalized(plugin.title, i18n.language) : name);
  const query = typeof window !== 'undefined' ? window.location.search : '';
  const src = useMemo(() => pluginIframeSrc(name, entryPath, deviceId, query), [name, entryPath, deviceId, query]);

  return (
    <div className="plg-page" ref={wrapRef}>
      <header className="plg-head">
        <span className="plg-title">
          <Icon size={18} aria-hidden="true" />
          <span className="ellipsis">{title}</span>
        </span>
        <span className="plg-dev">
          {devices.multi || devices.localOnly ? (
            <DeviceSwitcher perDevice />
          ) : device ? (
            <DeviceTag device={device} />
          ) : null}
        </span>
        <span className="grow" />
        <ConnChip />
        <Button variant="ghost" className="btn-sm" icon={isFull ? Minimize2 : Maximize2} aria-label={t(isFull ? 'plugins.exit_fullscreen' : 'plugins.fullscreen')} title={t(isFull ? 'plugins.exit_fullscreen' : 'plugins.fullscreen')} onClick={toggleFull} />
        <Button variant="ghost" className="btn-sm" icon={Settings2} aria-label={t('nav.plugins')} title={t('nav.plugins')} onClick={() => navigate('/plugins')} />
      </header>
      {!plugin ? (
        <div className="plg-pad">
          <Empty icon={Puzzle} title={t('plugins.not_installed_title')}>
            <p className="muted">{t('plugins.not_installed_body')}</p>
            <Button onClick={() => navigate('/plugins')}>{t('plugins.manage')}</Button>
          </Empty>
        </div>
      ) : plugin.incompatible ? (
        <div className="plg-pad">
          <Note kind="warn">{t(`plugins.incompat.${plugin.incompatible}`, { defaultValue: t('plugins.incompatible') })}</Note>
        </div>
      ) : (
        <iframe
          key={src}
          ref={frameRef}
          className="plg-frame"
          src={src}
          title={title}
          onLoad={sendInit}
          allow="clipboard-read; clipboard-write; fullscreen; autoplay"
        />
      )}
    </div>
  );
}
