// 远程指纹浏览器接入：当前设备通过 Profile 访问密钥绑定远端指纹浏览器（支持 HTTP / HTTPS）。
import { ExternalLink, Globe, KeyRound, Link2, RefreshCw, ShieldCheck, Unlink } from 'lucide-react';
import { useCallback, useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';

import { api, deviceApi } from './api.js';
import { errorText } from './i18n.js';
import { Row } from './page.jsx';
import { Button, Card, ErrorNote, Modal, useAction, useConfirm, useToast } from './ui.jsx';

const cache = new Map();
const TTL = 20_000;

export function invalidateBinding(device) {
  for (const key of cache.keys()) {
    if (device == null || key.startsWith(`${device ?? ''}:`)) cache.delete(key);
  }
}

export function cachedBinding(device) {
  return cache.get(`${device ?? ''}:1`)?.data ?? cache.get(`${device ?? ''}:0`)?.data ?? null;
}

export function fetchBinding(device, refresh = false) {
  const key = `${device ?? ''}:${refresh ? 1 : 0}`;
  const hit = cache.get(key);
  if (!refresh && hit && Date.now() - hit.at < TTL) return hit.job;
  const client = device != null ? deviceApi(device) : api;
  const job = client
    .get(refresh ? '/browser-binding?refresh=true' : '/browser-binding')
    .then(
      (d) => {
        const val = d && d.bound ? d : { bound: false };
        cache.set(key, { at: Date.now(), job: Promise.resolve(val), data: val });
        cache.set(`${device ?? ''}:0`, { at: Date.now(), job: Promise.resolve(val), data: val });
        return val;
      },
      () => {
        cache.delete(key);
        return { bound: false };
      },
    );
  cache.set(key, { at: Date.now(), job, data: hit?.data ?? null });
  return job;
}

function isLocalOrPrivateEndpoint(url) {
  try {
    const h = new URL(url).hostname;
    return (
      h === 'localhost' ||
      h === '127.0.0.1' ||
      h === '::1' ||
      h.endsWith('.local') ||
      /^10\./.test(h) ||
      /^192\.168\./.test(h) ||
      /^172\.(1[6-9]|2\d|3[01])\./.test(h)
    );
  } catch {
    return false;
  }
}

export function BrowserBindingModal({ open, device, initial, onClose, onSaved }) {
  const { t } = useTranslation();
  const toast = useToast();
  const confirm = useConfirm();
  const [endpoint, setEndpoint] = useState('');
  const [portalBase, setPortalBase] = useState('');
  const [accessKey, setAccessKey] = useState('');
  const [insecureTls, setInsecureTls] = useState(true);
  const [showAdv, setShowAdv] = useState(false);
  const [err, setErr] = useState(null);
  const { busy, run } = useAction();

  useEffect(() => {
    if (!open) return;
    setEndpoint(initial?.endpoint || '');
    setPortalBase(initial?.portal_base || '');
    setAccessKey('');
    setInsecureTls(initial?.insecure_tls ?? true);
    setShowAdv(Boolean(initial?.portal_base));
    setErr(null);
  }, [open, initial]);

  const isHttps = endpoint.trim().toLowerCase().startsWith('https://');
  const looksPrivate = isLocalOrPrivateEndpoint(endpoint.trim());
  const client = device != null ? deviceApi(device) : api;

  const submit = (e) => {
    e.preventDefault();
    setErr(null);
    run(async () => {
      try {
        const res = await client.put('/browser-binding', {
          endpoint: endpoint.trim(),
          portal_base: portalBase.trim(),
          access_key: accessKey.trim(),
          insecure_tls: isHttps ? insecureTls : false,
        });
        invalidateBinding(device);
        toast(t('browser_bind.saved', { name: res.profile_name || res.profile_id || '' }));
        onSaved?.(res);
        onClose();
      } catch (ex) {
        setErr(ex);
      }
    });
  };

  const unbind = async () => {
    const ok = await confirm({
      title: t('browser_bind.unbind_title'),
      body: t('browser_bind.unbind_body'),
      confirm: t('browser_bind.unbind'),
      danger: true,
    });
    if (!ok) return;
    run(async () => {
      try {
        await client.del('/browser-binding');
        invalidateBinding(device);
        toast(t('browser_bind.unbound'));
        onSaved?.({ bound: false });
        onClose();
      } catch (ex) {
        setErr(ex);
      }
    });
  };

  return (
    <Modal open={open} onClose={onClose} title={t('browser_bind.modal_title')}>
      <form className="browser-bind-form" onSubmit={submit}>
        <p className="muted browser-bind-desc">{t('browser_bind.card_desc')}</p>
        <label className="field">
          <span className="field-label">{t('browser_bind.endpoint')}</span>
          <input
            type="url"
            value={endpoint}
            onChange={(e) => setEndpoint(e.target.value)}
            placeholder={t('browser_bind.endpoint_placeholder')}
            required
            autoFocus
          />
          <span className="field-hint">{t('browser_bind.endpoint_hint')}</span>
        </label>

        <label className="field">
          <span className="field-label">{t('browser_bind.access_key')}</span>
          <input
            type="password"
            value={accessKey}
            onChange={(e) => setAccessKey(e.target.value)}
            placeholder={initial?.bound ? t('browser_bind.key_keep_hint', { masked: initial.key_masked || '****' }) : 'dbk_...'}
            required={!initial?.bound}
            autoComplete="off"
          />
        </label>

        {isHttps && (
          <label className="browser-bind-check">
            <input type="checkbox" checked={insecureTls} onChange={(e) => setInsecureTls(e.target.checked)} />
            <span>{t('browser_bind.insecure_tls')}</span>
          </label>
        )}

        {(showAdv || looksPrivate) ? (
          <label className="field">
            <span className="field-label">{t('browser_bind.portal_base')}</span>
            <input
              type="url"
              value={portalBase}
              onChange={(e) => setPortalBase(e.target.value)}
              placeholder={t('browser_bind.portal_base_placeholder')}
            />
            <span className="field-hint">{t('browser_bind.portal_base_hint')}</span>
          </label>
        ) : (
          <button type="button" className="link-button muted browser-bind-adv" onClick={() => setShowAdv(true)}>
            {t('browser_bind.show_portal_base')}
          </button>
        )}

        {err && <ErrorNote error={err} />}

        <div className="browser-bind-actions">
          {initial?.bound && (
            <Button type="button" variant="danger" icon={Unlink} disabled={busy} onClick={unbind}>
              {t('browser_bind.unbind')}
            </Button>
          )}
          <span className="grow" />
          <Button type="button" variant="ghost" onClick={onClose}>
            {t('action.cancel')}
          </Button>
          <Button type="submit" icon={ShieldCheck} busy={busy}>
            {t('browser_bind.test_and_save')}
          </Button>
        </div>
      </form>
    </Modal>
  );
}

export function BrowserBindingCard() {
  const { t } = useTranslation();
  const [data, setData] = useState(null);
  const [loading, setLoading] = useState(false);
  const [modalOpen, setModalOpen] = useState(false);

  const load = useCallback(async (refresh = false) => {
    setLoading(true);
    try {
      const res = await fetchBinding(null, refresh);
      setData(res);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    load(true);
  }, [load]);

  const statusLabel =
    data?.status === 'running'
      ? t('link_menu.status_running')
      : data?.status === 'starting'
        ? t('link_menu.status_starting')
        : data?.status === 'offline'
          ? t('link_menu.status_offline')
          : data?.status === 'invalid_key'
            ? t('link_menu.status_invalid_key')
            : t('link_menu.status_stopped');

  const statusTone =
    data?.status === 'running'
      ? 'ok'
      : data?.status === 'starting'
        ? 'warn'
        : data?.status === 'offline' || data?.status === 'invalid_key'
          ? 'bad'
          : 'idle';

  return (
    <>
      <Card
        id="fingerprint-browser"
        title={t('browser_bind.card_title')}
        sub={t('browser_bind.card_desc')}
        actions={
          data?.bound && (
            <Button variant="ghost" size="sm" icon={RefreshCw} busy={loading} onClick={() => load(true)}>
              {t('action.refresh')}
            </Button>
          )
        }
      >
        {data?.bound ? (
          <>
            <Row
              label={
                <span className="browser-bind-summary">
                  <Globe size={16} aria-hidden="true" />
                  <strong>{data.profile_name || data.profile_id}</strong>
                  <span className={`link-pop-pill is-${statusTone}`}>
                    <span className="link-pop-dot" />
                    {statusLabel}
                  </span>
                </span>
              }
              hint={`${data.endpoint} · ${data.key_masked || ''}`}
            >
              <div className="browser-bind-row-btns">
                {data.portal_url && (
                  <Button
                    variant="ghost"
                    size="sm"
                    icon={ExternalLink}
                    onClick={() => window.open(data.portal_url, '_blank', 'noopener,noreferrer')}
                  >
                    {t('browser_bind.open_portal')}
                  </Button>
                )}
                <Button variant="ghost" size="sm" icon={KeyRound} onClick={() => setModalOpen(true)}>
                  {t('browser_bind.edit')}
                </Button>
              </div>
            </Row>
          </>
        ) : (
          <Row label={t('browser_bind.not_bound')} hint={t('browser_bind.not_bound_hint')}>
            <Button icon={Link2} onClick={() => setModalOpen(true)}>
              {t('link_menu.bind_browser')}
            </Button>
          </Row>
        )}
      </Card>
      <BrowserBindingModal
        open={modalOpen}
        device={null}
        initial={data}
        onClose={() => setModalOpen(false)}
        onSaved={(next) => setData(next)}
      />
    </>
  );
}
