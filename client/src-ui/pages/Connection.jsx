// 设置 → 网络与访问：极简统一的连接与加速管理（局域网直连、一体化直连地址与证书管理、端口与网络诊断）及访问码安全。
// 普通用户零配置即可自动选最快路径（同网直连 → 自设地址 → 自动直连 → 中转）；
// 证书与直连地址就地绑定（反代自动免证书、公网 IP 自动尝试免费证书、DDNS 域名支持选系统文件夹自动识别与多域名 SNI 匹配）。
import { FolderOpen, KeyRound, Plus, RefreshCw, Search, ShieldCheck, X } from 'lucide-react';
import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';

import { SafetyNote, useStatus } from '../App.jsx';
import { api } from '../shared/api.js';
import { checkOpenHost, urlHost } from '../shared/host.js';
import { errorText } from '../shared/i18n.js';
import { More, Row } from '../shared/page.jsx';
import { Button, Card, CopyButton, ErrorNote, Field, FoldNote, Note, Skeleton, Switch, Time, useAction, useToast } from '../shared/ui.jsx';
import { FolderPicker } from './Terminals.jsx';

const iso = (s) => (s ? new Date(s * 1000).toISOString() : null);
const isPublicCodeOk = (code) => (code || '').replace(/[^a-zA-Z0-9]/g, '').length >= 12;

function extractHost(raw) {
  const noScheme = String(raw || '')
    .trim()
    .replace(/^https?:\/\//i, '')
    .split('/')[0];
  if (noScheme.startsWith('[')) {
    const end = noScheme.indexOf(']');
    return (end > 0 ? noScheme.slice(1, end) : noScheme.slice(1)).toLowerCase();
  }
  if ((noScheme.match(/:/g) || []).length === 1) return noScheme.split(':')[0].toLowerCase();
  return noScheme.toLowerCase();
}

function isIpv4(h) {
  return /^\d{1,3}(\.\d{1,3}){3}$/.test(h);
}

function isIpv6(h) {
  return Boolean(h) && h.includes(':');
}

function isPrivateOrLanIp(h) {
  if (!h) return false;
  if (/^(192\.168\.|10\.|172\.(1[6-9]|2\d|3[01])\.|100\.(6[4-9]|[7-9]\d|1[01]\d|12[0-7])\.)/.test(h)) return true;
  if (isIpv6(h) && /^f[cd]/i.test(h)) return true;
  return false;
}

function isPublicV4(h) {
  return isIpv4(h) && !isPrivateOrLanIp(h) && !h.startsWith('127.') && !h.startsWith('169.254.') && !h.startsWith('0.');
}

function parseEntry(raw) {
  const s = String(raw || '').trim();
  const explicitHttps = /^https:\/\//i.test(s);
  const explicitHttp = /^http:\/\//i.test(s);
  const host = extractHost(s);
  const v6 = isIpv6(host);
  const v4 = isIpv4(host);
  const lan = isPrivateOrLanIp(host);
  let kind = 'domain';
  if (explicitHttps) kind = 'proxy_https';
  else if (lan || explicitHttp) kind = 'lan_http';
  else if (v6) kind = 'ipv6';
  else if (v4) kind = 'ipv4';
  return { raw: s, host, kind, explicitHttps, explicitHttp };
}

function sanMatches(san, host) {
  const s = String(san || '').toLowerCase();
  const h = String(host || '').toLowerCase();
  if (!s || !h) return false;
  if (s === h) return true;
  if (s.startsWith('*.')) {
    const suffix = s.slice(1);
    return h.endsWith(suffix) && !h.slice(0, -suffix.length).includes('.');
  }
  return false;
}

function joinDir(dir, name) {
  if (!dir) return name;
  const sep = dir.includes('\\') && !dir.includes('/') ? '\\' : '/';
  return dir.endsWith('/') || dir.endsWith('\\') ? `${dir}${name}` : `${dir}${sep}${name}`;
}

function parentOf(path) {
  if (!path) return '';
  const i = Math.max(path.lastIndexOf('/'), path.lastIndexOf('\\'));
  return i > 0 ? path.slice(0, i) : '';
}

/** 从系统目录的文件列表里自动匹配证书链与私钥，优先匹配文件名包含目标域名的那一对（兼容 /etc/nginx/ssl 多站点同目录）。 */
function pickCertPairFromFiles(dir, entries, targetHost) {
  const files = (entries || []).filter((e) => !e.is_dir).map((e) => e.name);
  const hostLow = (targetHost || '').toLowerCase();
  const hostShort = hostLow.split('.')[0] || '';
  const hostBonus = (l) => {
    if (!hostLow) return 0;
    if (l.includes(hostLow)) return 60;
    if (hostShort.length >= 3 && l.includes(hostShort)) return 20;
    return 0;
  };
  const scoreCert = (name) => {
    const l = name.toLowerCase();
    if (l.includes('key') || l.startsWith('ca.') || l === 'chain.pem' || l === 'ca.cer' || l === 'ca.crt') return -1;
    let base = -1;
    if (l === 'fullchain.pem' || l === 'fullchain.cer') base = 100;
    else if (l.includes('fullchain')) base = 90;
    else if (l === 'cert.pem' || l === 'certificate.pem' || l === 'server.crt' || l === 'server.pem') base = 70;
    else if (l.endsWith('.crt') || l.endsWith('.cer') || l.endsWith('.pem')) base = 50;
    return base > 0 ? base + hostBonus(l) : -1;
  };
  const scoreKey = (name) => {
    const l = name.toLowerCase();
    let base = -1;
    if (l === 'privkey.pem' || l === 'private.key' || l === 'key.pem' || l === 'server.key') base = 100;
    else if (l.endsWith('.key')) base = 80;
    else if (l.includes('key') && l.endsWith('.pem')) base = 70;
    return base > 0 ? base + hostBonus(l) : -1;
  };
  const bestCert = files
    .map((f) => [f, scoreCert(f)])
    .filter((x) => x[1] > 0)
    .sort((a, b) => b[1] - a[1])[0]?.[0];
  const bestKey = files
    .map((f) => [f, scoreKey(f)])
    .filter((x) => x[1] > 0)
    .sort((a, b) => b[1] - a[1])[0]?.[0];
  return {
    certPath: bestCert ? joinDir(dir, bestCert) : '',
    keyPath: bestKey ? joinDir(dir, bestKey) : '',
    certName: bestCert || '',
    keyName: bestKey || '',
  };
}

function Snippet({ text }) {
  return (
    <span className="fw-cmd">
      <code className="mono" style={{ whiteSpace: 'pre' }}>
        {text}
      </code>
      <CopyButton text={text} label={false} variant="plain" />
    </span>
  );
}

function WindowsFirewall({ exe }) {
  const { t } = useTranslation();
  const program = exe || '$env:LOCALAPPDATA\\Looklook\\looklook.exe';
  const ps = `New-NetFirewallRule -DisplayName "Looklook LAN" -Direction Inbound -Action Allow -Profile Private -Program "${program}"`;
  return (
    <details className="fw">
      <summary>{t('access.fw_title')}</summary>
      <p className="small muted">{t('access.fw_why')}</p>
      <ol className="fw-steps small">
        <li>{t('access.fw_local')}</li>
        <li>{t('access.fw_lan')}</li>
        <li>{t('access.fw_profile')}</li>
        <li>
          {t('access.fw_fix')}
          <span className="fw-cmd">
            <code className="mono">{ps}</code>
            <CopyButton text={ps} label={false} variant="plain" />
          </span>
        </li>
        <li>{t('access.fw_never')}</li>
      </ol>
    </details>
  );
}

function ListenPort({ data, save, busy, error, clearError, reload }) {
  const { t } = useTranslation();
  const toast = useToast();
  const saved = data.custom_port ? String(data.custom_port) : '';
  const [val, setVal] = useState(saved);
  const [restarting, setRestarting] = useState(false);

  useEffect(() => {
    setVal(data.custom_port ? String(data.custom_port) : '');
  }, [data.custom_port]);

  const trimmed = val.trim();
  const parsed = trimmed === '' ? null : /^\d+$/.test(trimmed) ? Number(trimmed) : NaN;
  const badPort = trimmed !== '' && (!Number.isInteger(parsed) || parsed < 1 || parsed > 65535);
  const changed = trimmed !== saved;
  const pendingPort = !data.listen_override && data.custom_port && data.custom_port !== data.port ? data.custom_port : null;

  const modeLabel = data.listen_override
    ? t('conn_cfg.port_mode_override')
    : data.custom_port
      ? t('conn_cfg.port_mode_custom')
      : t('conn_cfg.port_mode_auto');

  const submit = async (e) => {
    e.preventDefault();
    if (badPort || data.listen_override) return;
    await save({ custom_port: parsed });
  };

  const doRestart = async () => {
    if (restarting) return;
    setRestarting(true);
    try {
      await api.post('/restart', {});
      toast(t('conn_cfg.port_restarting'));
      const nextPort = pendingPort;
      if (nextPort && window.location.port === String(data.port)) {
        const host = window.location.hostname.includes(':') ? `[${window.location.hostname}]` : window.location.hostname;
        const target = `${window.location.protocol}//${host}:${nextPort}${window.location.pathname}${window.location.search}${window.location.hash}`;
        setTimeout(() => {
          window.location.href = target;
        }, 2000);
      } else {
        setTimeout(() => {
          reload();
          setRestarting(false);
        }, 3000);
      }
    } catch (err) {
      setRestarting(false);
      toast(errorText(t, err));
    }
  };

  return (
    <form className="field" onSubmit={submit}>
      <b>{t('conn_cfg.port_title')}</b>
      <span className="field-hint">{t('conn_cfg.port_hint', { port: data.port, mode: modeLabel })}</span>
      <div className="row">
        <input
          value={val}
          onChange={(e) => {
            setVal(e.target.value);
            if (error) clearError();
          }}
          className="mono"
          style={{ maxWidth: 160 }}
          inputMode="numeric"
          autoComplete="off"
          spellCheck={false}
          disabled={data.listen_override || restarting}
          placeholder={t('conn_cfg.port_ph', { port: data.port })}
          aria-label={t('conn_cfg.port_title')}
          aria-invalid={badPort || error ? 'true' : undefined}
        />
        <Button type="submit" busy={busy} disabled={data.listen_override || restarting || !changed || badPort}>
          {t('action.save')}
        </Button>
        {data.custom_port && !data.listen_override && (
          <Button
            variant="ghost"
            busy={busy}
            disabled={restarting}
            onClick={() => {
              if (error) clearError();
              save({ custom_port: null });
            }}
          >
            {t('conn_cfg.port_auto')}
          </Button>
        )}
      </div>
      {badPort && <span className="field-error">{t('error:rule.port')}</span>}
      {!badPort && error && <span className="field-error">{t(`error:rule.${error.params?.rule}`, { defaultValue: errorText(t, error) })}</span>}
      {data.listen_override && <span className="field-hint">{t('conn_cfg.port_override_hint')}</span>}
      {pendingPort && (
        <Note kind="info">
          {t('conn_cfg.port_pending', { next: pendingPort, cur: data.port })}{' '}
          <button type="button" className="btn btn-plain" style={{ minHeight: 28, padding: '0 4px' }} disabled={restarting} onClick={doRestart}>
            {restarting ? t('conn_cfg.port_restarting') : t('conn_cfg.port_restart')}
          </button>
        </Note>
      )}
    </form>
  );
}

const NAT_KIND = { open: 'ok', punch: 'ok', partial: 'warn', relay_only: 'info', wrong_addr: 'warn', proxy: 'info' };

function NatResult({ r, ports }) {
  const { t } = useTranslation();
  if (!r) return null;
  const key = r.verdict === 'unknown' || r.verdict === 'wrong_addr' ? `${r.verdict}_${r.why}` : r.verdict;
  const vars = { exit: r.exit, ip: r.public_v4, begin: ports[0], end: ports[1] };
  return (
    <FoldNote
      kind={NAT_KIND[r.verdict] ?? 'info'}
      summary={
        <>
          {t(`direct.nat.${key}`, { ...vars, defaultValue: t('direct.nat.unknown_no_reply') })}{' '}
          <span className="muted small">
            · <Time value={new Date(r.at * 1000).toISOString()} only="relative" />
          </span>
        </>
      }
    >
      <p className="small">{t(`direct.nat.${key}_hint`, { ...vars, defaultValue: '' })}</p>
      <span className="field-hint direct-probes">
        {(r.ports ?? []).map((p) => (
          <span key={p.port} className="direct-probe">
            <span>{t('direct.nat.from_outside')}</span>
            <span className="mono">
              {r.a_target}:{p.port} · {t(p.open ? 'direct.nat.open_port' : 'direct.nat.closed_port')}
            </span>
          </span>
        ))}
        <span className="direct-probe">
          <span>{t('direct.nat.local_port')}</span>
          <span className="mono">
            {r.local_port || '—'}
            {!r.in_range && r.local_port ? ` · ${t('direct.nat.random_port')}` : ''}
          </span>
        </span>
      </span>
    </FoldNote>
  );
}

function NetDiagnosis({ net, onSaved }) {
  const { t } = useTranslation();
  const toast = useToast();
  const [f, setF] = useState(() => ({
    port_begin: String(net.port_begin),
    port_end: String(net.port_end),
    public_v4: net.public_v4 || '',
    public_v6: net.public_v6 || '',
  }));
  const { busy, error, run } = useAction();
  const detect = useAction();

  useEffect(() => {
    setF({
      port_begin: String(net.port_begin),
      port_end: String(net.port_end),
      public_v4: net.public_v4 || '',
      public_v6: net.public_v6 || '',
    });
  }, [net.port_begin, net.port_end, net.public_v4, net.public_v6]);

  const save = async (body) => {
    const r = await run(() => api.put('/rtc/net', body));
    if (r) {
      toast(t('common.saved'));
      onSaved(r);
    }
  };

  const [defBegin, defEnd] = net.default_ports ?? [50000, 50031];
  const isDefault = f.port_begin === String(defBegin) && f.port_end === String(defEnd);
  const hasLegacyAddr = Boolean(net.public_v4 || net.public_v6);
  const changed =
    f.port_begin !== String(net.port_begin) ||
    f.port_end !== String(net.port_end) ||
    f.public_v4 !== (net.public_v4 || '') ||
    f.public_v6 !== (net.public_v6 || '');
  const fieldError = (name) => (error?.params?.field === name ? t(`error:rule.${error.params.rule}`) : undefined);

  const runDetect = async () => {
    const r = await detect.run(() => api.post('/rtc/net/detect', {}));
    if (!r) return;
    if (r.nat) onSaved({ ...net, nat: r.nat });
  };

  return (
    <div className="field" style={{ marginBottom: 0 }}>
      <div className="between" style={{ marginBottom: 8 }}>
        <span className="field-hint">{net.nat ? '' : t('conn_cfg.diag_idle')}</span>
        <Button variant="secondary" icon={Search} busy={detect.busy} onClick={runDetect}>
          {t('direct.detect')}
        </Button>
      </div>
      <NatResult r={net.nat} ports={[net.port_begin, net.port_end]} />
      <ErrorNote error={detect.error} />
      <form
        onSubmit={(e) => {
          e.preventDefault();
          save({
            port_begin: Number(f.port_begin),
            port_end: Number(f.port_end),
            ...(hasLegacyAddr ? { public_v4: f.public_v4.trim(), public_v6: f.public_v6.trim() } : {}),
          });
        }}
      >
        <Field label={t('conn_cfg.udp_ports')} hint={t('conn_cfg.udp_ports_hint')} error={fieldError('rtc_ports')}>
          <span className="row direct-ports">
            <input
              value={f.port_begin}
              onChange={(e) => setF({ ...f, port_begin: e.target.value })}
              inputMode="numeric"
              className="mono"
              aria-label={t('direct.port_begin')}
            />
            <span aria-hidden="true">–</span>
            <input
              value={f.port_end}
              onChange={(e) => setF({ ...f, port_end: e.target.value })}
              inputMode="numeric"
              className="mono"
              aria-label={t('direct.port_end')}
            />
            {!isDefault && (
              <Button variant="plain" onClick={() => setF({ ...f, port_begin: String(defBegin), port_end: String(defEnd) })}>
                {t('direct.ports_default')}
              </Button>
            )}
          </span>
        </Field>
        {hasLegacyAddr && (
          <>
            <Field label={t('direct.public_v4')} error={fieldError('public_v4')}>
              <input value={f.public_v4} onChange={(e) => setF({ ...f, public_v4: e.target.value })} className="mono" placeholder={t('direct.not_set')} spellCheck={false} />
            </Field>
            <Field label={t('direct.public_v6')} error={fieldError('public_v6')}>
              <input value={f.public_v6} onChange={(e) => setF({ ...f, public_v6: e.target.value })} className="mono" placeholder={t('direct.not_set')} spellCheck={false} />
            </Field>
          </>
        )}
        {changed && (
          <div className="row" style={{ marginBottom: 10 }}>
            <Button type="submit" busy={busy}>
              {t('action.save')}
            </Button>
          </div>
        )}
      </form>
      <Row label={t('conn_cfg.prefer_ipv6')} hint={t('direct.ipv6_hint')}>
        <Switch checked={net.ipv6} onChange={(v) => save({ ipv6: v })} disabled={busy} label={t('conn_cfg.prefer_ipv6')} />
      </Row>
      <ErrorNote error={error?.params?.field ? null : error} />
    </div>
  );
}

function EndpointCard({ url, certs, directStatus, busy, probe, probing, onProbe, onRemove, onPickDir, onSaveManualCert, onClearCert, onPickFileForManual, manualDraft, onManualChange, manualOpen, onToggleManual }) {
  const { t } = useTranslation();
  const p = parseEntry(url);
  const matchingCert = (certs || []).find((c) => c.ok && (c.sans || []).some((san) => sanMatches(san, p.host)));
  const brokenCert = !matchingCert && (certs || []).find((c) => !c.ok && c.cert_path);
  const acmeHit = (p.kind === 'ipv4' || p.kind === 'ipv6') && directStatus?.ip?.toLowerCase() === p.host;
  const acmeOk = acmeHit && directStatus?.phase === 'ok';
  const acmeWorking = acmeHit && directStatus?.phase === 'working';
  const href = p.explicitHttps || p.explicitHttp ? `${url}/` : p.kind === 'lan_http' ? `http://${url}/` : matchingCert || acmeOk ? `https://${url}/` : null;

  let tagText = t('conn_cfg.tag_domain');
  let tagCls = 'badge';
  let footCls = 'is-muted';
  let footText = '';
  let showCertPick = false;
  let showCertManage = false;

  if (p.kind === 'proxy_https') {
    tagText = t('conn_cfg.tag_proxy');
    tagCls = 'badge badge-ok';
    footCls = 'is-ok';
    footText = t('conn_cfg.ep_proxy_ok');
  } else if (p.kind === 'lan_http') {
    tagText = t('conn_cfg.tag_lan');
    tagCls = 'badge';
    footCls = 'is-muted';
    footText = t('conn_cfg.ep_lan_ok');
  } else if (matchingCert) {
    tagText = t('conn_cfg.tag_https_ok');
    tagCls = 'badge badge-ok';
    footCls = 'is-ok';
    footText = t('conn_cfg.ep_cert_ok', { sans: (matchingCert.sans || []).slice(0, 3).join(', ') });
    showCertManage = true;
  } else if (acmeOk) {
    tagText = t('conn_cfg.tag_https_ok');
    tagCls = 'badge badge-ok';
    footCls = 'is-ok';
    footText = t('conn_cfg.ep_acme_ok');
  } else if (p.kind === 'ipv4' || p.kind === 'ipv6') {
    tagText = p.kind === 'ipv6' ? t('conn_cfg.tag_ipv6') : t('conn_cfg.tag_ipv4');
    footCls = 'is-muted';
    footText = acmeWorking ? t('conn_cfg.ep_acme_working') : t('conn_cfg.ep_ip_app_only');
    showCertPick = true;
  } else if (brokenCert) {
    tagText = t('conn_cfg.tag_cert_err');
    tagCls = 'badge badge-warn';
    footCls = 'is-warn';
    footText = t('conn_cfg.ep_cert_err');
    showCertPick = true;
  } else {
    tagText = t('conn_cfg.tag_domain');
    footCls = 'is-muted';
    footText = t('conn_cfg.ep_domain_app_only');
    showCertPick = true;
  }

  const probeText = probe
    ? probe.code === 'ok'
      ? t('conn_cfg.probe_ok', { ms: probe.ms })
      : probe.code === 'tls_untrusted'
        ? t('conn_cfg.probe_app_only', { ms: probe.ms })
        : probe.code === 'host_rejected'
          ? t('conn_cfg.probe_host_rejected')
          : probe.code === 'wrong_target'
            ? t('conn_cfg.probe_wrong_target', { detail: probe.detail || '' })
            : t('conn_cfg.probe_unreachable')
    : '';

  return (
    <div className="ep-card">
      <div className="ep-main">
        {href ? (
          <a className="mono ellipsis grow" href={href} target="_blank" rel="noopener noreferrer">
            {url}
          </a>
        ) : (
          <span className="mono ellipsis grow">{url}</span>
        )}
        <span className={tagCls}>{tagText}</span>
        <button type="button" className="btn btn-plain ep-btn" disabled={probing} onClick={() => onProbe(url)}>
          {probing ? t('conn_cfg.probing') : t('conn_cfg.probe')}
        </button>
        <CopyButton text={href || url} label={false} variant="plain" />
        <button type="button" className="btn btn-plain ep-icon" aria-label={t('conn_cfg.custom_remove', { url })} disabled={busy} onClick={() => onRemove(url)}>
          <X size={15} />
        </button>
      </div>
      <div className={`ep-foot ${footCls}`}>
        <span className="ep-foot-status">
          {footCls === 'is-ok' && <ShieldCheck size={14} aria-hidden="true" style={{ flex: 'none' }} />}
          <span>
            {footText}
            {matchingCert?.not_after && (
              <>
                {' · '}
                {t('conn_cfg.cert_until')} <Time value={iso(matchingCert.not_after)} />
              </>
            )}
            {acmeOk && directStatus?.not_after && (
              <>
                {' · '}
                {t('conn_cfg.cert_until')} <Time value={iso(directStatus.not_after)} />
              </>
            )}
          </span>
        </span>
        <span className="ep-foot-acts">
          {showCertPick && (
            <>
              <button type="button" className="btn btn-secondary ep-btn" disabled={busy} onClick={() => onPickDir(url, p.host)}>
                <FolderOpen size={14} aria-hidden="true" />
                {t('conn_cfg.pick_cert_dir')}
              </button>
              <button type="button" className="btn btn-plain ep-btn" disabled={busy} onClick={() => onToggleManual(url)}>
                {manualOpen ? t('action.cancel') : t('conn_cfg.pick_cert_manual')}
              </button>
            </>
          )}
          {showCertManage && (
            <>
              <button type="button" className="btn btn-plain ep-btn" disabled={busy} onClick={() => onPickDir(url, p.host)}>
                {t('conn_cfg.cert_replace')}
              </button>
              <button type="button" className="btn btn-plain ep-btn" disabled={busy} onClick={() => onClearCert(matchingCert)}>
                {t('conn_cfg.cert_clear')}
              </button>
            </>
          )}
        </span>
      </div>
      {probe && (
        <div className={`ep-probe ${probe.ok && !probe.app_only ? 'is-ok' : probe.ok ? 'is-muted' : 'is-warn'}`}>
          {probeText}
        </div>
      )}
      {manualOpen && (
        <div className="ep-cert-edit">
          <div className="row">
            <input
              value={manualDraft.cert_path}
              onChange={(e) => onManualChange({ ...manualDraft, cert_path: e.target.value })}
              className="mono"
              placeholder={t('conn_cfg.cert_file_ph')}
              aria-label={t('conn_cfg.cert_file')}
              spellCheck={false}
            />
            <Button variant="ghost" onClick={() => onPickFileForManual(url, 'cert')}>
              {t('conn_cfg.browse')}
            </Button>
          </div>
          <div className="row">
            <input
              value={manualDraft.key_path}
              onChange={(e) => onManualChange({ ...manualDraft, key_path: e.target.value })}
              className="mono"
              placeholder={t('conn_cfg.key_file_ph')}
              aria-label={t('conn_cfg.key_file')}
              spellCheck={false}
            />
            <Button variant="ghost" onClick={() => onPickFileForManual(url, 'key')}>
              {t('conn_cfg.browse')}
            </Button>
          </div>
          <div className="row" style={{ justifyContent: 'flex-end' }}>
            <Button variant="ghost" onClick={() => onToggleManual(url)}>
              {t('action.cancel')}
            </Button>
            <Button
              busy={busy}
              disabled={!manualDraft.cert_path.trim() || !manualDraft.key_path.trim()}
              onClick={() => onSaveManualCert(url, manualDraft.cert_path.trim(), manualDraft.key_path.trim())}
            >
              {t('action.save')}
            </Button>
          </div>
        </div>
      )}
    </div>
  );
}

function AddEndpointBox({ port, customPort, listenOverride, busy, onPinPort, onCancel, onSubmit, draftCert, onPickDraftDir, onPickDraftFile, onChangeDraftCert, onClearDraftCert }) {
  const { t } = useTranslation();
  const [mode, setMode] = useState('proxy'); // 'proxy' | 'direct'
  const [hostVal, setHostVal] = useState('');
  const [portVal, setPortVal] = useState('');
  const [manualCert, setManualCert] = useState(false);

  const cleanHost = extractHost(hostVal);
  const directIsIp = isIpv4(cleanHost) || isIpv6(cleanHost);
  const showPinHint = !customPort && !listenOverride;

  const caddyCfg = `code.example.com {\n    reverse_proxy 127.0.0.1:${port}\n}`;
  const nginxCfg = `location / {\n    proxy_pass http://127.0.0.1:${port};\n    proxy_http_version 1.1;\n    proxy_set_header Host $http_host;\n    proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;\n    proxy_set_header X-Forwarded-Proto $scheme;\n    proxy_set_header Upgrade $http_upgrade;\n    proxy_set_header Connection "upgrade";\n}`;

  const submit = async (e) => {
    e.preventDefault();
    const raw = hostVal.trim();
    if (!raw) return;
    let formatted = '';
    if (mode === 'proxy') {
      formatted = /^https?:\/\//i.test(raw) ? raw.replace(/\/+$/, '') : `https://${raw.replace(/\/+$/, '')}`;
    } else {
      const h = extractHost(raw);
      if (!h) return;
      const noScheme = raw.replace(/^https?:\/\//i, '').split('/')[0];
      const inlinePort = !noScheme.startsWith('[') && (noScheme.match(/:/g) || []).length === 1 ? noScheme.split(':')[1] : noScheme.match(/\]:(\d+)$/)?.[1];
      const p = portVal.trim() || inlinePort || String(port);
      const hostLit = isIpv6(h) ? `[${h}]` : h;
      formatted = `${hostLit}:${p}`;
    }
    const certPair =
      mode === 'direct' && draftCert?.cert_path?.trim() && draftCert?.key_path?.trim()
        ? { cert_path: draftCert.cert_path.trim(), key_path: draftCert.key_path.trim() }
        : null;
    await onSubmit(formatted, certPair);
  };

  return (
    <form className="conn-add-box" onSubmit={submit}>
      {showPinHint && (
        <div className="conn-pin-hint small">
          <span>{t('conn_cfg.pin_port_hint', { port })}</span>
          <button type="button" className="link-btn" disabled={busy} onClick={onPinPort}>
            {t('conn_cfg.pin_port_btn', { port })}
          </button>
        </div>
      )}

      <div>
        <div className="field-label" style={{ marginBottom: 6 }}>
          {t('conn_cfg.add_mode_q')}
        </div>
        <div className="conn-mode-grid">
          <button type="button" className={`conn-mode-card ${mode === 'proxy' ? 'active' : ''}`} onClick={() => setMode('proxy')}>
            <b>{t('conn_cfg.mode_proxy')}</b>
            <span>{t('conn_cfg.mode_proxy_desc')}</span>
          </button>
          <button type="button" className={`conn-mode-card ${mode === 'direct' ? 'active' : ''}`} onClick={() => setMode('direct')}>
            <b>{t('conn_cfg.mode_direct')}</b>
            <span>{t('conn_cfg.mode_direct_desc')}</span>
          </button>
        </div>
      </div>

      {mode === 'proxy' ? (
        <>
          <Field label={t('conn_cfg.proxy_url_label')} hint={t('conn_cfg.proxy_url_hint')}>
            <input
              value={hostVal}
              onChange={(e) => setHostVal(e.target.value)}
              className="mono"
              inputMode="url"
              autoComplete="off"
              autoCapitalize="off"
              spellCheck={false}
              placeholder={t('conn_cfg.proxy_url_ph')}
            />
          </Field>
          <details className="fw" style={{ marginBottom: 0 }}>
            <summary>{t('conn_cfg.proxy_guide', { port })}</summary>
            <p className="small muted">{t('conn_cfg.proxy_guide_caddy')}</p>
            <Snippet text={caddyCfg} />
            <p className="small muted" style={{ marginTop: 8 }}>
              {t('conn_cfg.proxy_guide_nginx')}
            </p>
            <Snippet text={nginxCfg} />
          </details>
        </>
      ) : (
        <>
          <div className="row" style={{ alignItems: 'flex-start', flexWrap: 'wrap' }}>
            <div className="field grow" style={{ minWidth: 200, marginBottom: 0 }}>
              <label className="field-label" htmlFor="conn-direct-host">
                {t('conn_cfg.direct_host_label')}
              </label>
              <input
                id="conn-direct-host"
                value={hostVal}
                onChange={(e) => setHostVal(e.target.value)}
                className="mono"
                autoComplete="off"
                autoCapitalize="off"
                spellCheck={false}
                placeholder={t('conn_cfg.direct_host_ph')}
              />
            </div>
            <div className="field" style={{ width: 120, flex: 'none', marginBottom: 0 }}>
              <label className="field-label" htmlFor="conn-direct-port">
                {t('conn_cfg.direct_port_label')}
              </label>
              <input
                id="conn-direct-port"
                value={portVal}
                onChange={(e) => setPortVal(e.target.value)}
                className="mono"
                inputMode="numeric"
                placeholder={String(port)}
                spellCheck={false}
              />
            </div>
          </div>
          {directIsIp ? (
            <span className="field-hint">{t('conn_cfg.direct_ip_hint')}</span>
          ) : (
            <div className="field" style={{ marginBottom: 0 }}>
              <span className="field-label">{t('conn_cfg.direct_cert_opt')}</span>
              <div className="conn-cert-pick-row">
                <span className={`small ${draftCert?.cert_path ? 'conn-ok-text' : 'muted'}`}>
                  {draftCert?.cert_path ? t('conn_cfg.draft_cert_picked', { cert: draftCert.cert_path }) : t('conn_cfg.direct_cert_hint')}
                </span>
                <span className="inline">
                  <Button variant="secondary" icon={FolderOpen} onClick={() => onPickDraftDir(cleanHost)}>
                    {t('conn_cfg.pick_cert_dir')}
                  </Button>
                  <button type="button" className="btn btn-plain ep-btn" onClick={() => setManualCert((v) => !v)}>
                    {manualCert ? t('action.cancel') : t('conn_cfg.pick_cert_manual')}
                  </button>
                  {draftCert?.cert_path && (
                    <button type="button" className="btn btn-plain ep-btn" onClick={onClearDraftCert}>
                      {t('conn_cfg.cert_clear')}
                    </button>
                  )}
                </span>
              </div>
              {manualCert && (
                <div className="ep-cert-edit" style={{ marginTop: 6, borderRadius: 10 }}>
                  <div className="row">
                    <input
                      value={draftCert?.cert_path || ''}
                      onChange={(e) => onChangeDraftCert({ cert_path: e.target.value, key_path: draftCert?.key_path || '' })}
                      className="mono"
                      placeholder={t('conn_cfg.cert_file_ph')}
                      spellCheck={false}
                    />
                    <Button variant="ghost" onClick={() => onPickDraftFile('cert')}>
                      {t('conn_cfg.browse')}
                    </Button>
                  </div>
                  <div className="row">
                    <input
                      value={draftCert?.key_path || ''}
                      onChange={(e) => onChangeDraftCert({ cert_path: draftCert?.cert_path || '', key_path: e.target.value })}
                      className="mono"
                      placeholder={t('conn_cfg.key_file_ph')}
                      spellCheck={false}
                    />
                    <Button variant="ghost" onClick={() => onPickDraftFile('key')}>
                      {t('conn_cfg.browse')}
                    </Button>
                  </div>
                </div>
              )}
            </div>
          )}
        </>
      )}

      <div className="row" style={{ justifyContent: 'flex-end' }}>
        <Button type="button" variant="ghost" onClick={onCancel}>
          {t('action.cancel')}
        </Button>
        <Button type="submit" busy={busy} disabled={!hostVal.trim() || Boolean(draftCert?.cert_path?.trim()) !== Boolean(draftCert?.key_path?.trim())}>
          {t('access.list_add')}
        </Button>
      </div>
    </form>
  );
}

function CodeBox({ data, save, busy }) {
  const { t } = useTranslation();
  const [custom, setCustom] = useState(null);
  return (
    <div className="access-code">
      <div className="access-code-row">
        <span className="access-code-value mono">{data.code}</span>
        <CopyButton text={data.code} label={false} variant="ghost" />
        <Button variant="ghost" icon={RefreshCw} busy={busy} onClick={() => save({ new_code: true })}>
          {t('access.code_new')}
        </Button>
        {custom === null && (
          <Button variant="plain" onClick={() => setCustom('')}>
            {t('access.code_custom')}
          </Button>
        )}
      </div>
      {custom !== null && (
        <form
          className="row"
          onSubmit={async (e) => {
            e.preventDefault();
            if (await save({ code: custom })) setCustom(null);
          }}
        >
          <input
            value={custom}
            onChange={(e) => setCustom(e.target.value)}
            className="mono"
            autoComplete="off"
            spellCheck={false}
            placeholder={t('access.code_custom_placeholder')}
            aria-label={t('access.code_custom')}
          />
          <Button type="submit" busy={busy} disabled={custom.replace(/[\s-]/g, '').length < 8}>
            {t('action.save')}
          </Button>
          <Button type="button" variant="ghost" onClick={() => setCustom(null)}>
            {t('action.cancel')}
          </Button>
        </form>
      )}
      <span className="field-hint">{t('access.code_forgot')}</span>
    </div>
  );
}

function Whitelist({ data, save, busy, error, clearError }) {
  const { t } = useTranslation();
  const [ip, setIp] = useState('');
  const add = async (e) => {
    e.preventDefault();
    if (await save({ allowed_ips: [...data.allowed_ips, ip.trim()] })) setIp('');
  };
  return (
    <div className="field">
      <b>{t('access.list_title')}</b>
      <span className="field-hint">{t('access.list_hint')}</span>
      {data.allowed_ips.length > 0 && (
        <div className="chips">
          {data.allowed_ips.map((x) => (
            <span key={x} className="chip chip-x mono">
              {x}
              {data.resolved?.[x] && <span className="muted">{data.resolved[x].length ? `→ ${data.resolved[x].join(' ')}` : t('access.list_unresolved')}</span>}
              <button type="button" aria-label={t('access.list_remove', { ip: x })} disabled={busy} onClick={() => save({ allowed_ips: data.allowed_ips.filter((y) => y !== x) })}>
                <X size={14} />
              </button>
            </span>
          ))}
        </div>
      )}
      <form className="row" onSubmit={add}>
        <input
          value={ip}
          onChange={(e) => {
            setIp(e.target.value);
            if (error) clearError();
          }}
          className="mono"
          inputMode="url"
          autoComplete="off"
          autoCapitalize="off"
          spellCheck={false}
          placeholder={t('access.list_placeholder')}
          aria-label={t('access.list_title')}
          aria-invalid={error ? 'true' : undefined}
        />
        <Button type="submit" variant="secondary" icon={Plus} busy={busy} disabled={!ip.trim()}>
          {t('access.list_add')}
        </Button>
      </form>
      {error && <span className="field-error">{t(`error:rule.${error.params.rule}`, { defaultValue: errorText(t, error) })}</span>}
    </div>
  );
}

function OpenHost({ data, save, busy }) {
  const { t } = useTranslation();
  const [value, setValue] = useState(data.open_host);
  useEffect(() => setValue(data.open_host), [data.open_host]);
  const rule = checkOpenHost(value);
  const examples = t('access.open_host_example_list').split('|');
  return (
    <form
      className="field"
      onSubmit={(e) => {
        e.preventDefault();
        save({ open_host: value });
      }}
    >
      <label className="field-label" htmlFor="open-host">
        {t('access.open_host')}
      </label>
      <span className="field-hint">{t('access.open_host_hint')}</span>
      <div className="row">
        <input
          id="open-host"
          value={value}
          onChange={(e) => setValue(e.target.value)}
          className="mono"
          placeholder={t('access.open_host_placeholder')}
          inputMode="url"
          autoComplete="off"
          autoCapitalize="off"
          spellCheck={false}
          aria-invalid={rule ? 'true' : undefined}
        />
        <Button type="submit" busy={busy} disabled={Boolean(rule) || value.trim() === data.open_host}>
          {t('action.save')}
        </Button>
      </div>
      {rule ? (
        <span className="field-error">{t(`error:rule.${rule}`)}</span>
      ) : (
        <span className="field-hint">{t('access.open_host_url', { url: `http://${value.trim() ? urlHost(value) : '127.0.0.1'}:${data.port}/` })}</span>
      )}
      <div className="chips" aria-label={t('access.open_host_examples')}>
        <span className="small muted">{t('access.open_host_examples')}</span>
        {examples.map((x) => (
          <button key={x} type="button" className="chip mono" onClick={() => setValue(x)}>
            {x}
          </button>
        ))}
      </div>
    </form>
  );
}

export default function ConnectionCards() {
  const { t } = useTranslation();
  const { status, reload } = useStatus();
  const toast = useToast();
  const [data, setData] = useState(null);
  const [loadError, setLoadError] = useState(null);
  const connAct = useAction();
  const accessAct = useAction();

  // 添加面板 & 证书选择弹窗 & 连通性测试状态
  const [addOpen, setAddOpen] = useState(false);
  const [draftCert, setDraftCert] = useState(null);
  const [manualUrl, setManualUrl] = useState(null);
  const [manualDraft, setManualDraft] = useState({ cert_path: '', key_path: '' });
  const [picker, setPicker] = useState(null); // { mode: 'dir'|'file', target: 'draft'|url, which?: 'cert'|'key', host?: string, initial?: string }
  const [probes, setProbes] = useState({});
  const [probingUrl, setProbingUrl] = useState(null);

  const load = () => api.get('/connection').then(setData, setLoadError);
  useEffect(() => {
    load();
  }, []);

  const phase = data?.direct_status?.phase;
  useEffect(() => {
    if (phase !== 'working') return undefined;
    const id = setInterval(() => {
      api.get('/connection').then(setData, () => {});
    }, 3000);
    return () => clearInterval(id);
  }, [phase]);

  if (loadError) return <ErrorNote error={loadError} />;
  if (!data) return <Skeleton height={280} />;

  const saveConn = async (body, silentToast = false) => {
    const r = await connAct.run(() => api.put('/connection', body));
    if (r) {
      setData(r);
      if (!silentToast) toast(t('common.saved'));
      reload();
    }
    return r;
  };

  const saveAccess = async (body) => {
    const r = await accessAct.run(() => api.put('/access', body));
    if (r) {
      setData((cur) => ({ ...cur, ...r, code_ok: isPublicCodeOk(r.code) }));
      toast(t('common.saved'));
      reload();
    }
    return Boolean(r);
  };

  const onSavedNet = (nextNet) => {
    setData((cur) => ({ ...cur, net: nextNet }));
    api.get('/connection').then(setData, () => {});
  };

  const curCertPairs = () =>
    (data.tls_certs || []).map((c) => ({ cert_path: c.cert_path, key_path: c.key_path }));

  const upsertCertPair = async (certPath, keyPath, targetHost) => {
    const existing = curCertPairs().filter((c) => c.cert_path !== certPath || c.key_path !== keyPath);
    const next = [...existing, { cert_path: certPath, key_path: keyPath }];
    const r = await saveConn({ tls_certs: next }, true);
    if (!r) return false;
    const loaded = (r.tls_certs || []).find((c) => c.cert_path === certPath && c.key_path === keyPath);
    if (targetHost && loaded?.ok && !(loaded.sans || []).some((san) => sanMatches(san, targetHost))) {
      toast(t('conn_cfg.cert_mismatch_toast', { host: targetHost, sans: (loaded.sans || []).join(', ') || '—' }));
    } else {
      toast(t('conn_cfg.cert_loaded_toast'));
    }
    return true;
  };

  const handleClearCert = async (certObj) => {
    const next = curCertPairs().filter((c) => c.cert_path !== certObj.cert_path || c.key_path !== certObj.key_path);
    await saveConn({ tls_certs: next });
  };

  const handlePickResult = async (pickedPath) => {
    const ctx = picker;
    setPicker(null);
    if (!ctx || !pickedPath) return;

    if (ctx.mode === 'dir') {
      try {
        const res = await api.get(`/fs/list?path=${encodeURIComponent(pickedPath)}&files=1&hidden=1`);
        const found = pickCertPairFromFiles(res.path || pickedPath, res.entries, ctx.host);
        if (found.certPath && found.keyPath) {
          if (ctx.target === 'draft') {
            setDraftCert({ cert_path: found.certPath, key_path: found.keyPath });
            toast(t('conn_cfg.cert_dir_found', { cert: found.certName, key: found.keyName }));
          } else {
            await upsertCertPair(found.certPath, found.keyPath, ctx.host);
            setManualUrl(null);
          }
        } else {
          toast(t('conn_cfg.cert_dir_not_found'));
          if (ctx.target === 'draft') {
            setDraftCert({ cert_path: found.certPath, key_path: found.keyPath });
          } else {
            setManualUrl(ctx.target);
            setManualDraft({ cert_path: found.certPath, key_path: found.keyPath });
          }
        }
      } catch (err) {
        toast(errorText(t, err));
      }
      return;
    }

    // 单文件选择
    if (ctx.target === 'draft') {
      setDraftCert((cur) => ({
        cert_path: ctx.which === 'cert' ? pickedPath : cur?.cert_path || '',
        key_path: ctx.which === 'key' ? pickedPath : cur?.key_path || '',
      }));
    } else {
      setManualDraft((cur) => ({
        cert_path: ctx.which === 'cert' ? pickedPath : cur.cert_path,
        key_path: ctx.which === 'key' ? pickedPath : cur.key_path,
      }));
    }
  };

  const runProbe = async (url) => {
    setProbingUrl(url);
    try {
      const r = await api.post('/connection/test', { url });
      setProbes((cur) => ({ ...cur, [url]: r }));
    } catch (err) {
      toast(errorText(t, err));
    } finally {
      setProbingUrl(null);
    }
  };

  const handleAddSubmit = async (url, certPair) => {
    const urls = [...(data.custom_urls ?? []), url];
    const body = { custom_urls: urls };
    if (certPair) {
      const existing = curCertPairs().filter((c) => c.cert_path !== certPair.cert_path || c.key_path !== certPair.key_path);
      body.tls_certs = [...existing, certPair];
    }
    const r = await saveConn(body);
    if (r) {
      setAddOpen(false);
      setDraftCert(null);
    }
  };

  const handleRemoveUrl = async (url) => {
    const next = (data.custom_urls ?? []).filter((x) => x !== url);
    await saveConn({ custom_urls: next });
  };

  const st = data.direct_status || {};
  const net = data.net || { port_begin: 50000, port_end: 50031, default_ports: [50000, 50031], public_v4: '', public_v6: '', ipv6: true, nic_v6: [] };
  const customUrls = data.custom_urls ?? [];
  const lanIps = data.lan_ips ?? [];
  const lanUrls = lanIps.map((ip) => ({ ip, url: `http://${urlHost(ip)}:${data.port}/`, isMesh: ip.startsWith('100.') }));

  // Docker 容器下：如果当前浏览器是通过宿主机局域网 IP / 域名打开的，给一键添加
  const browserHost = typeof window !== 'undefined' ? window.location.hostname : '';
  const browserHostPort = typeof window !== 'undefined' ? window.location.host : '';
  const browserIsLoopback = !browserHost || browserHost === 'localhost' || browserHost === '127.0.0.1' || browserHost === '::1' || browserHost === '[::1]';
  const dockerQuickUrl = data.in_container && lanUrls.length === 0 && !browserIsLoopback ? `${window.location.protocol}//${browserHostPort}` : null;
  const dockerQuickAvailable = dockerQuickUrl && !customUrls.includes(dockerQuickUrl);

  // 智能检测到的外网地址（一键添加胶囊，避免手打长 IPv6）
  const quickChips = [];
  const detectedV6 = net.nic_v6?.[0];
  if (detectedV6) {
    const cand = `[${detectedV6}]:${data.port}`;
    if (!customUrls.includes(cand)) quickChips.push({ label: t('conn_cfg.quick_v6', { ip: detectedV6 }), value: cand });
  }
  const detectedV4 = [net.resolved_v4, net.public_v4, net.nat?.exit].find((x) => isPublicV4(x || ''));
  if (detectedV4) {
    const cand = `${detectedV4}:${data.port}`;
    if (!customUrls.includes(cand)) quickChips.push({ label: t('conn_cfg.quick_v4', { ip: detectedV4 }), value: cand });
  }

  // 兼容旧版开启了 public_direct 的用户（不破坏已有配置）
  const legacyPubEndpoints = data.public_direct
    ? (data.endpoints ?? []).filter((e) => e.kind !== 'lan' && !customUrls.some((u) => e.url.endsWith(u) || e.url === u))
    : [];

  const hasExternalUrl = data.public_direct || customUrls.some((u) => parseEntry(u).kind !== 'lan_http');
  const urlError = connAct.error?.params?.field === 'custom_urls' ? connAct.error : null;
  const certError = connAct.error?.params?.field === 'tls_cert' ? connAct.error : null;
  const portError = connAct.error?.params?.field === 'listen_port' ? connAct.error : null;
  const listError = accessAct.error?.params?.field === 'allowed_ips' ? accessAct.error : null;
  const pendingPort = !data.listen_override && data.custom_port && data.custom_port !== data.port;

  return (
    <>
      <div id="public-https" className="anchor">
        <Card title={t('conn_cfg.title')} sub={t('conn_cfg.intro')}>
          {/* 1. 局域网直连 */}
          <Row label={t('conn_cfg.lan')} hint={t('conn_cfg.lan_hint')}>
            <Switch checked={data.allow_lan} onChange={(v) => saveConn({ allow_lan: v })} disabled={connAct.busy} label={t('conn_cfg.lan')} />
          </Row>
          {data.allow_lan && (
            <div className="access-sub">
              {lanUrls.length > 0 ? (
                lanUrls.map((item) => (
                  <span key={item.url} className="one-line">
                    <a className="mono ellipsis" href={item.url} target="_blank" rel="noopener noreferrer">
                      {item.url}
                    </a>
                    <span className="badge">{t(item.isMesh ? 'conn_cfg.tag_mesh' : 'conn_cfg.tag_lan')}</span>
                    <CopyButton text={item.url} label={false} variant="plain" />
                  </span>
                ))
              ) : data.in_container ? (
                <Note kind="info">
                  <div>
                    <div>{t('conn_cfg.docker_lan_hint')}</div>
                    {dockerQuickAvailable && (
                      <div style={{ marginTop: 8 }}>
                        <Button
                          variant="secondary"
                          icon={Plus}
                          busy={connAct.busy}
                          onClick={() => saveConn({ custom_urls: [...customUrls, dockerQuickUrl] })}
                        >
                          {t('conn_cfg.docker_quick_add', { host: browserHostPort })}
                        </Button>
                      </div>
                    )}
                  </div>
                </Note>
              ) : (
                <span className="field-hint">{t('access.lan_unknown', { port: data.port })}</span>
              )}
            </div>
          )}
          {status.capabilities.os === 'windows' && data.allow_lan && <WindowsFirewall exe={data.exe} />}

          {/* 2. 自定义直连地址（地址 + 证书 + 测试一体化） */}
          <div className="conn-section">
            <div className="between" style={{ alignItems: 'flex-start' }}>
              <div className="grow">
                <b style={{ display: 'block', fontSize: 15 }}>{t('conn_cfg.custom_title')}</b>
                <span className="field-hint">{t('conn_cfg.custom_hint')}</span>
              </div>
              {!addOpen && (
                <Button variant="secondary" icon={Plus} onClick={() => setAddOpen(true)}>
                  {t('conn_cfg.add_btn')}
                </Button>
              )}
            </div>

            {customUrls.length > 0 && (
              <div className="ep-list">
                {customUrls.map((u) => (
                  <EndpointCard
                    key={u}
                    url={u}
                    certs={data.tls_certs}
                    directStatus={st}
                    busy={connAct.busy}
                    probe={probes[u]}
                    probing={probingUrl === u}
                    onProbe={runProbe}
                    onRemove={handleRemoveUrl}
                    onPickDir={(targetUrl, host) => setPicker({ mode: 'dir', target: targetUrl, host })}
                    onClearCert={handleClearCert}
                    manualOpen={manualUrl === u}
                    manualDraft={manualDraft}
                    onManualChange={setManualDraft}
                    onToggleManual={(targetUrl) => {
                      if (manualUrl === targetUrl) {
                        setManualUrl(null);
                      } else {
                        setManualUrl(targetUrl);
                        setManualDraft({ cert_path: '', key_path: '' });
                      }
                    }}
                    onPickFileForManual={(targetUrl, which) =>
                      setPicker({
                        mode: 'file',
                        target: targetUrl,
                        which,
                        initial: parentOf(which === 'key' ? manualDraft.key_path || manualDraft.cert_path : manualDraft.cert_path),
                      })
                    }
                    onSaveManualCert={async (targetUrl, cp, kp) => {
                      if (await upsertCertPair(cp, kp, parseEntry(targetUrl).host)) {
                        setManualUrl(null);
                      }
                    }}
                  />
                ))}
              </div>
            )}

            {legacyPubEndpoints.length > 0 && (
              <Note kind="info">
                <div className="grow">
                  <div>{t('conn_cfg.legacy_pub_note', { urls: legacyPubEndpoints.map((e) => e.url).join('、') })}</div>
                  <button
                    type="button"
                    className="link-btn"
                    style={{ marginTop: 4 }}
                    disabled={connAct.busy}
                    onClick={() => saveConn({ public_direct: false })}
                  >
                    {t('conn_cfg.legacy_pub_off')}
                  </button>
                </div>
              </Note>
            )}

            {quickChips.length > 0 && (
              <div className="chips" style={{ marginTop: 10 }}>
                <span className="field-hint">{t('conn_cfg.quick_label')}</span>
                {quickChips.map((c) => (
                  <button
                    key={c.value}
                    type="button"
                    className="chip conn-quick-chip"
                    disabled={connAct.busy}
                    onClick={() => saveConn({ custom_urls: [...customUrls, c.value] })}
                  >
                    <Plus size={13} aria-hidden="true" />
                    {c.label}
                  </button>
                ))}
              </div>
            )}

            {addOpen && (
              <AddEndpointBox
                port={data.port}
                customPort={data.custom_port}
                listenOverride={data.listen_override}
                busy={connAct.busy}
                onPinPort={() => saveConn({ custom_port: data.port })}
                onCancel={() => {
                  setAddOpen(false);
                  setDraftCert(null);
                }}
                onSubmit={handleAddSubmit}
                draftCert={draftCert}
                onPickDraftDir={(host) => setPicker({ mode: 'dir', target: 'draft', host })}
                onPickDraftFile={(which) =>
                  setPicker({
                    mode: 'file',
                    target: 'draft',
                    which,
                    initial: parentOf(which === 'key' ? draftCert?.key_path || draftCert?.cert_path : draftCert?.cert_path),
                  })
                }
                onChangeDraftCert={setDraftCert}
                onClearDraftCert={() => setDraftCert(null)}
              />
            )}

            {urlError && <span className="field-error">{t(`error:rule.${urlError.params?.rule}`, { defaultValue: errorText(t, urlError) })}</span>}
            {certError && <span className="field-error">{t(`error:rule.${certError.params?.rule}`, { defaultValue: errorText(t, certError) })}</span>}
          </div>

          {/* 3. 更多设置：固定端口 + 最深层网络诊断 */}
          <More summary={t('conn_cfg.more_settings')} open={Boolean(portError || pendingPort)}>
            <ListenPort
              data={data}
              save={saveConn}
              busy={connAct.busy}
              error={portError}
              clearError={() => connAct.setError(null)}
              reload={() => {
                load();
                reload();
              }}
            />
            <More summary={t('conn_cfg.net_diag_more')}>
              <NetDiagnosis net={net} onSaved={onSavedNet} />
            </More>
          </More>
          <ErrorNote error={urlError || certError || portError ? null : connAct.error} />
        </Card>
      </div>

      <div id="access" className="anchor">
        <Card title={t('conn_cfg.sec_title')} sub={t('conn_cfg.sec_intro')}>
          <CodeBox data={data} save={saveAccess} busy={accessAct.busy} />
          {!data.code_ok && hasExternalUrl && (
            <Note kind="warn" icon={KeyRound}>
              {t('public_https.code_short')}{' '}
              <button type="button" className="btn btn-plain" style={{ minHeight: 28, padding: '0 4px' }} onClick={() => saveAccess({ new_code: true })}>
                {t('access.code_new')}
              </button>
            </Note>
          )}

          <Row label={t('conn_cfg.lan_code')} hint={t('conn_cfg.lan_code_hint')}>
            <Switch checked={data.code_enabled} onChange={(v) => saveAccess({ code_enabled: v })} disabled={accessAct.busy} label={t('conn_cfg.lan_code')} />
          </Row>

          {(data.logins ?? []).length > 0 && (
            <div className="field">
              <b>{t('public_https.logins')}</b>
              {data.logins.map((l) => (
                <span key={`${l.ip}-${l.at}`} className="field-hint mono" style={{ display: 'block' }}>
                  {l.ip} · {t(`public_https.via.${l.via}`, { defaultValue: l.via })} · <Time value={new Date(l.at).toISOString()} only="relative" />
                </span>
              ))}
            </div>
          )}

          <More summary={t('access.more')} open={(data.allowed_ips ?? []).length > 0 || Boolean(data.open_host)}>
            <Whitelist data={data} save={saveAccess} busy={accessAct.busy} error={listError} clearError={() => accessAct.setError(null)} />
            {(data.allowed_ips ?? []).length > 0 && !data.code_enabled && (
              <Note kind="warn" icon={KeyRound}>
                {t('access.list_no_code')}{' '}
                <button type="button" className="btn btn-plain" style={{ minHeight: 28, padding: '0 4px' }} onClick={() => saveAccess({ code_enabled: true })}>
                  {t('access.code_enable')}
                </button>
              </Note>
            )}
            <OpenHost data={data} save={saveAccess} busy={accessAct.busy} />
          </More>
          <ErrorNote error={listError ? null : accessAct.error} />
          <SafetyNote full />
        </Card>
      </div>

      {/* 系统文件夹 / 文件选择器（选择证书目录或单文件） */}
      <FolderPicker
        open={Boolean(picker)}
        files={picker?.mode === 'file'}
        initial={picker?.initial}
        title={
          picker?.mode === 'dir'
            ? t('conn_cfg.picker_dir_title', { host: picker?.host || '' })
            : picker?.which === 'key'
              ? t('conn_cfg.key_file')
              : t('conn_cfg.cert_file')
        }
        pickLabel={picker?.mode === 'dir' ? t('conn_cfg.picker_dir_pick') : undefined}
        onClose={() => setPicker(null)}
        onPick={handlePickResult}
      />
    </>
  );
}
