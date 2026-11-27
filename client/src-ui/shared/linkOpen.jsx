// 全会话统一链接悬浮菜单（对话 UI + 终端 iframe）：
// - 零配置：点击链接即弹出精美悬浮动作面板（桌面端智能定位，移动端底部抽屉）；
// - 未绑定指纹浏览器：本机打开 / 复制链接 / 绑定指纹浏览器（原位弹窗完成绑定）；
// - 已绑定指纹浏览器：在指纹浏览器中打开（含实时状态点；若未启动则弹窗确认启动并打开）/ 本机打开 / 复制链接；
// - ⌘ / Ctrl + 点击同样弹菜单（不再直开），右键与长按保持原生菜单。
import { Copy, ExternalLink, Globe, Link2, Settings2, ShieldAlert, X } from 'lucide-react';
import { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { useTranslation } from 'react-i18next';

import { useStatus } from '../App.jsx';
import { api, deviceApi } from './api.js';
import { BrowserBindingModal, cachedBinding, fetchBinding, invalidateBinding } from './BrowserBinding.jsx';
import { copyText } from './chatCopy.js';
import { CLAUDE_DOMAINS, matchClaudeDomain } from './claudeDomains.js';
import { errorText } from './i18n.js';
import { isDesktopApp } from './layers.js';
import { Row } from './page.jsx';
import { SyncLabel } from './SettingsSync.jsx';
import { termFocus } from './termFrame.js';
import { Card, ErrorNote, Switch, useAction, useConfirm, useToast } from './ui.jsx';

const isWebUrl = (u) => /^https?:\/\//i.test(u);

function isMobileViewport() {
  return window.innerWidth <= 640 || window.matchMedia?.('(pointer: coarse)').matches;
}

function splitUrlPreview(url) {
  try {
    const u = new URL(url);
    const tail = `${u.pathname === '/' ? '' : u.pathname}${u.search}${u.hash}`;
    return { host: u.host, tail };
  } catch {
    return { host: url, tail: '' };
  }
}

export function useLinkOpen(device, onClosed) {
  const { t } = useTranslation();
  const { status } = useStatus();
  const toast = useToast();
  const confirm = useConfirm();
  const [menu, setMenu] = useState(null);
  const [bindModal, setBindModal] = useState(null);
  const [busy, setBusy] = useState(false);
  const guardEnabled = status?.settings?.claude_link_guard !== false;

  useEffect(() => {
    fetchBinding(device, false);
  }, [device]);

  const closeMenu = useCallback(() => {
    setMenu(null);
    onClosed?.();
  }, [onClosed]);

  const openInFingerprint = useCallback(
    async (url, bindingSnapshot) => {
      if (busy) return;
      const client = deviceApi(device);
      const notifyOpened = (res) => {
        const portal = res?.portal_url || bindingSnapshot?.portal_url;
        const msg = res?.started ? t('link_menu.started_opened_toast') : t('link_menu.opened_toast');
        if (portal) {
          toast(msg, 'ok', {
            label: t('link_menu.view_portal'),
            onClick: () => window.open(portal, '_blank', 'noopener,noreferrer'),
          });
        } else {
          toast(msg);
        }
      };
      setBusy(true);
      try {
        const res = await client.post('/browser-binding/open-url', { url, auto_start: false });
        if (res?.result === 'opened') {
          notifyOpened(res);
          return;
        }
        if (res?.result === 'stopped') {
          const name = res.profile_name || bindingSnapshot?.profile_name || t('browser_bind.card_title');
          const ok = await confirm({
            title: t('link_menu.start_confirm_title'),
            body: t(res.needs_restart ? 'link_menu.restart_confirm_body' : 'link_menu.start_confirm_body', { name }),
            confirm: t(res.needs_restart ? 'link_menu.restart_confirm_ok' : 'link_menu.start_confirm_ok'),
          });
          if (!ok) return;
          toast(t('link_menu.starting_toast'));
          const startedRes = await client.post('/browser-binding/open-url', { url, auto_start: true });
          invalidateBinding(device);
          fetchBinding(device, true);
          notifyOpened(startedRes);
          return;
        }
      } catch (e) {
        if (e?.code === 'BROWSER_NOT_BOUND') {
          invalidateBinding(device);
          setBindModal({ initial: { bound: false }, pendingUrl: url });
        } else {
          toast(errorText(t, e), 'error');
        }
      } finally {
        setBusy(false);
      }
    },
    [busy, device, confirm, toast, t],
  );

  const openMenuAt = useCallback(
    (url, x, y, opts = {}) => {
      if (!isWebUrl(url)) return false;
      try {
        if (new URL(url).origin === window.location.origin) return false;
      } catch {
        return false;
      }
      const initial = cachedBinding(device) || { bound: false };
      setMenu({ url, x, y, top: opts.top ?? y - 8, binding: initial });
      fetchBinding(device, true).then((fresh) => {
        setMenu((cur) => (cur && cur.url === url ? { ...cur, binding: fresh } : cur));
      });
      return true;
    },
    [device],
  );

  const onClickCapture = useCallback(
    (e) => {
      if (e.defaultPrevented || e.button !== 0) return;
      if (e.target.closest?.('.md img, .link-pop, .link-pop-backdrop')) return;
      const a = e.target.closest?.('a[href]');
      if (!a || a.hasAttribute('download') || a.classList.contains('md-file') || a.closest('[contenteditable="true"]')) return;
      const url = a.href;
      if (!isWebUrl(url)) return;
      try {
        if (new URL(url).origin === window.location.origin) return;
      } catch {
        return;
      }
      e.preventDefault();
      e.stopPropagation();
      const r = a.getBoundingClientRect();
      openMenuAt(url, r.left, r.bottom + 6, { top: r.top - 6 });
    },
    [openMenuAt],
  );

  const openLocalWithGuard = useCallback(
    async (url) => {
      const { host } = splitUrlPreview(url);
      const ok = await confirm({
        title: t('link_menu.claude_local_confirm_title'),
        body: <span className="link-guard-confirm">{t('link_menu.claude_local_confirm_body', { host })}</span>,
        confirm: t('link_menu.claude_local_confirm_ok'),
        danger: true,
      });
      if (!ok) return;
      window.open(url, '_blank', 'noopener,noreferrer');
    },
    [confirm, t],
  );

  const claudeDomain = menu && guardEnabled ? matchClaudeDomain(menu.url) : null;

  const node = (
    <>
      {menu && (
        <LinkActionPopover
          {...menu}
          claudeDomain={claudeDomain}
          busy={busy}
          onClose={closeMenu}
          onFingerprint={() => openInFingerprint(menu.url, menu.binding)}
          onLocalOpen={claudeDomain ? () => openLocalWithGuard(menu.url) : null}
          onBind={() => setBindModal({ initial: menu.binding, pendingUrl: menu.url })}
          onCopy={async () => {
            if (await copyText(menu.url)) toast(t(claudeDomain ? 'link_menu.claude_copied' : 'link_menu.copied'));
            else window.prompt('', menu.url);
          }}
        />
      )}
      <BrowserBindingModal
        open={Boolean(bindModal)}
        device={device}
        initial={bindModal?.initial}
        onClose={() => {
          setBindModal(null);
          onClosed?.();
        }}
        onSaved={(next) => {
          if (next?.bound && bindModal?.pendingUrl) {
            openInFingerprint(bindModal.pendingUrl, next);
          }
        }}
      />
    </>
  );

  return { onClickCapture, openMenuAt, menu: node };
}

/** 终端 iframe 内点击链接（term-inject.js 发 `looklook:open-link`）转交给统一悬浮菜单。 */
export function useTerminalLinkOpen(frame, device, enabled = true) {
  const refocus = useCallback(() => {
    if (enabled && !isMobileViewport()) termFocus(frame);
  }, [enabled, frame]);
  const links = useLinkOpen(device, refocus);
  const openRef = useRef(links.openMenuAt);
  openRef.current = links.openMenuAt;

  useEffect(() => {
    if (!enabled) return undefined;
    const onMsg = (e) => {
      if (e.data?.type !== 'looklook:open-link') return;
      if (!frame.current || e.source !== frame.current.contentWindow) return;
      const url = String(e.data.url || '');
      if (!url) return;
      const rect = frame.current.getBoundingClientRect();
      const x = rect.left + (Number(e.data.x) || 24);
      const y = rect.top + (Number(e.data.y) || 24);
      openRef.current(url, x, y + 10, { top: y - 10, mod: Boolean(e.data.mod) });
    };
    window.addEventListener('message', onMsg);
    return () => window.removeEventListener('message', onMsg);
  }, [enabled, frame]);

  return links;
}

function LinkActionPopover({ url, x, y, top, binding, claudeDomain, busy, onClose, onFingerprint, onLocalOpen, onBind, onCopy }) {
  const { t } = useTranslation();
  const ref = useRef(null);
  const sheet = isMobileViewport();
  const [pos, setPos] = useState({ left: x, top: y, visibility: 'hidden' });
  const claudeWarn = Boolean(claudeDomain);

  useLayoutEffect(() => {
    if (sheet) return;
    const el = ref.current;
    if (!el) return;
    const w = el.offsetWidth;
    const h = el.offsetHeight;
    const below = y + h + 10 <= window.innerHeight;
    setPos({
      left: Math.max(10, Math.min(x, window.innerWidth - w - 10)),
      top: Math.max(10, below ? y : Math.max(10, top - h)),
      visibility: 'visible',
    });
  }, [sheet, x, y, top, binding, claudeWarn]);

  useEffect(() => {
    const away = (e) => {
      if (!ref.current?.contains(e.target)) onClose();
    };
    const onKey = (e) => {
      if (e.key === 'Escape') {
        e.preventDefault();
        onClose();
        return;
      }
      if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
        const items = Array.from(ref.current?.querySelectorAll('[role="menuitem"]') || []);
        if (!items.length) return;
        e.preventDefault();
        const idx = items.indexOf(document.activeElement);
        const next = e.key === 'ArrowDown' ? (idx + 1) % items.length : (idx - 1 + items.length) % items.length;
        items[next]?.focus();
      }
    };
    document.addEventListener('pointerdown', away, true);
    document.addEventListener('keydown', onKey);
    window.addEventListener('resize', onClose);
    window.addEventListener('scroll', onClose, true);
    return () => {
      document.removeEventListener('pointerdown', away, true);
      document.removeEventListener('keydown', onKey);
      window.removeEventListener('resize', onClose);
      window.removeEventListener('scroll', onClose, true);
    };
  }, [onClose]);

  useEffect(() => {
    ref.current?.querySelector('[role="menuitem"]')?.focus({ preventScroll: true });
  }, []);

  const { host, tail } = splitUrlPreview(url);
  const bound = Boolean(binding?.bound);
  const status = binding?.status;
  const statusText =
    status === 'running'
      ? t('link_menu.status_running')
      : status === 'starting'
        ? t('link_menu.status_starting')
        : status === 'offline'
          ? t('link_menu.status_offline')
          : status === 'invalid_key'
            ? t('link_menu.status_invalid_key')
            : t('link_menu.status_stopped');
  const statusTone =
    status === 'running'
      ? 'ok'
      : status === 'starting'
        ? 'warn'
        : status === 'offline' || status === 'invalid_key'
          ? 'bad'
          : 'idle';

  const pick = (fn) => () => {
    onClose();
    fn();
  };

  return createPortal(
    <>
      {sheet && <div className="link-pop-backdrop" onClick={onClose} aria-hidden="true" />}
      <div
        ref={ref}
        className={`link-pop ${sheet ? 'link-pop-sheet' : 'link-pop-fixed'}${claudeWarn ? ' is-claude-warn' : ''}`}
        role="menu"
        style={sheet ? undefined : pos}
        aria-label={url}
      >
        {sheet && <div className="link-pop-grab" aria-hidden="true" />}
        <div className="link-pop-head" title={url}>
          {claudeWarn && (
            <span className="link-pop-warn-ico" aria-hidden="true">
              <ShieldAlert size={15} />
            </span>
          )}
          <span className="link-pop-url">
            <strong className="link-pop-host">{host}</strong>
            {tail && <span className="link-pop-path">{tail}</span>}
          </span>
          {bound && (
            <button
              type="button"
              className="link-pop-gear"
              title={t('browser_bind.edit')}
              aria-label={t('browser_bind.edit')}
              onClick={pick(onBind)}
            >
              <Settings2 size={14} aria-hidden="true" />
            </button>
          )}
          {sheet && (
            <button type="button" className="link-pop-gear" aria-label={t('action.close')} onClick={onClose}>
              <X size={16} aria-hidden="true" />
            </button>
          )}
        </div>

        {claudeWarn && (
          <div className="link-pop-warn" role="alert">
            <div className="link-pop-warn-title">
              <ShieldAlert size={14} aria-hidden="true" />
              <span>{t('link_menu.claude_warn_title')}</span>
            </div>
            <p className="link-pop-warn-desc">
              {t(bound ? 'link_menu.claude_warn_desc_bound' : 'link_menu.claude_warn_desc_unbound', { domain: claudeDomain })}
            </p>
          </div>
        )}

        <div className="link-pop-body">
          {bound && (
            <button
              type="button"
              role="menuitem"
              className="link-pop-btn is-primary"
              disabled={busy}
              onClick={pick(onFingerprint)}
            >
              <span className="link-pop-ico">
                <Globe size={17} aria-hidden="true" />
              </span>
              <span className="link-pop-txt">
                <span className="link-pop-title">{t('link_menu.open_fingerprint')}</span>
                <span className="link-pop-sub">{binding.profile_name || binding.profile_id || t('browser_bind.card_title')}</span>
              </span>
              <span className={`link-pop-pill is-${statusTone}`}>
                <span className="link-pop-dot" />
                {statusText}
              </span>
            </button>
          )}

          {claudeWarn ? (
            <button
              type="button"
              role="menuitem"
              className="link-pop-btn is-danger-action"
              onClick={pick(onLocalOpen)}
            >
              <span className="link-pop-ico">
                <ExternalLink size={17} aria-hidden="true" />
              </span>
              <span className="link-pop-txt">
                <span className="link-pop-title">{t(isDesktopApp ? 'link_menu.open_local_desktop' : 'link_menu.open_local')}</span>
                <span className="link-pop-sub is-warn-sub">{t('link_menu.claude_local_hint')}</span>
              </span>
            </button>
          ) : (
            <a
              role="menuitem"
              className={`link-pop-btn ${bound ? '' : 'is-primary'}`}
              href={url}
              target="_blank"
              rel="noopener noreferrer"
              onClick={onClose}
            >
              <span className="link-pop-ico">
                <ExternalLink size={17} aria-hidden="true" />
              </span>
              <span className="link-pop-txt">
                <span className="link-pop-title">{t(isDesktopApp ? 'link_menu.open_local_desktop' : 'link_menu.open_local')}</span>
                <span className="link-pop-sub">{t('link_menu.open_local_hint')}</span>
              </span>
            </a>
          )}

          <button type="button" role="menuitem" className={`link-pop-btn${claudeWarn ? ' is-warn-copy' : ''}`} onClick={pick(onCopy)}>
            <span className="link-pop-ico">
              <Copy size={16} aria-hidden="true" />
            </span>
            <span className="link-pop-txt">
              <span className="link-pop-title">{t('link_menu.copy')}</span>
              {claudeWarn && <span className="link-pop-sub is-warn-sub">{t('link_menu.claude_copy_hint')}</span>}
            </span>
          </button>

          {!bound && (
            <>
              <div className="link-pop-sep" role="separator" />
              <button type="button" role="menuitem" className="link-pop-btn is-bind" onClick={pick(onBind)}>
                <span className="link-pop-ico">
                  <Link2 size={16} aria-hidden="true" />
                </span>
                <span className="link-pop-txt">
                  <span className="link-pop-title">{t('link_menu.bind_browser')}</span>
                  <span className="link-pop-sub">{t('link_menu.bind_browser_hint')}</span>
                </span>
              </button>
            </>
          )}
        </div>
      </div>
    </>,
    document.body,
  );
}

/** 设置页“通用”里的系统拦截开关：Claude 域名链接风控拦截（默认开启，关闭需二次确认）。 */
export function ClaudeLinkGuardCard() {
  const { t, i18n } = useTranslation();
  const { status, reload } = useStatus();
  const confirm = useConfirm();
  const toast = useToast();
  const { busy, error, run } = useAction();
  const enabled = status?.settings?.claude_link_guard !== false;
  const sep = i18n.language?.startsWith('en') ? ', ' : '、';

  const onToggle = async (next) => {
    if (!next) {
      const ok = await confirm({
        title: t('link_guard.disable_title'),
        body: <span className="link-guard-confirm">{t('link_guard.disable_body')}</span>,
        confirm: t('link_guard.disable_confirm'),
        danger: true,
      });
      if (!ok) return;
    }
    const res = await run(() => api.put('/settings', { claude_link_guard: next }));
    if (res) {
      reload();
      toast(t(next ? 'link_guard.enabled_toast' : 'link_guard.disabled_toast'));
    }
  };

  return (
    <Card id="claude-link-guard" title={t('link_guard.card_title')} sub={t('link_guard.card_desc')}>
      <Row
        label={<SyncLabel text={t('link_guard.toggle')} />}
        hint={
          enabled
            ? t('link_guard.state_on', { domains: CLAUDE_DOMAINS.slice(0, 3).join(sep) })
            : t('link_guard.state_off')
        }
      >
        <Switch checked={enabled} disabled={busy} onChange={onToggle} label={t('link_guard.toggle')} />
      </Row>
      <ErrorNote error={error} />
    </Card>
  );
}
