// 设置 → 网络与访问：统一管理连接加速（局域网直连、公网 IPv6/IPv4 直连、穿透与加速地址、高级证书与网络参数）与访问码安全。
// 默认零配置即可按“本机 → 局域网 → 穿透 / IPv6 / 公网直连 → P2P → 中转兜底”自动选最快路径；
// 局域网直连默认不强制访问码（避免误锁），公网与穿透地址始终强制要求 12 位以上访问码或看看平台登录票据。
import { Globe, KeyRound, Plus, RefreshCw, Search, X } from 'lucide-react';
import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';

import { SafetyNote, useStatus } from '../App.jsx';
import { api } from '../shared/api.js';
import { checkOpenHost, urlHost } from '../shared/host.js';
import { errorText } from '../shared/i18n.js';
import { More, Row } from '../shared/page.jsx';
import { Button, Card, CopyButton, ErrorNote, Field, FoldNote, Note, Skeleton, Switch, Time, useAction, useToast } from '../shared/ui.jsx';

const iso = (s) => (s ? new Date(s * 1000).toISOString() : null);
const isName = (s) => /[a-z]/i.test(s) && !s.includes(':');
const isPublicCodeOk = (code) => (code || '').replace(/[^a-zA-Z0-9]/g, '').length >= 12;
const netForm = (d) => ({
  port_begin: String(d.port_begin),
  port_end: String(d.port_end),
  public_v4: d.public_v4 || '',
  public_v6: d.public_v6 || '',
});

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

function Failure({ st, port }) {
  const { t } = useTranslation();
  const code = st?.error;
  if (!code) return null;
  const nginx = `location /.well-known/acme-challenge/ {\n    proxy_pass http://127.0.0.1:${port};\n}`;
  const caddy = `handle /.well-known/acme-challenge/* {\n    reverse_proxy 127.0.0.1:${port}\n}`;
  return (
    <div style={{ marginTop: 8 }}>
      <b className="small" style={{ display: 'block' }}>
        {t(`public_https.err.${code}`, { defaultValue: t('public_https.err.other', { code }) })}
      </b>
      {(code === 'port_busy' || code === 'no_privilege') && (
        <span className="field-hint" style={{ display: 'block' }}>
          {t('public_https.forward_hint')}
          <Snippet text={nginx} />
          <Snippet text={caddy} />
        </span>
      )}
      {code === 'no_privilege' && (
        <span className="field-hint" style={{ display: 'block' }}>
          {t('public_https.privilege_hint')}
          <Snippet text="sudo setcap cap_net_bind_service=+ep $(command -v looklook)" />
          <Snippet text="sudo sysctl net.ipv4.ip_unprivileged_port_start=80" />
        </span>
      )}
      {st.detail && (
        <span className="field-hint mono" style={{ display: 'block' }}>
          {st.detail}
        </span>
      )}
      {st.next_try && (
        <span className="field-hint" style={{ display: 'block' }}>
          {t('public_https.next_try')} <Time value={iso(st.next_try)} only="relative" />
        </span>
      )}
    </div>
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

function CustomUrls({ urls, save, busy, error, clearError }) {
  const { t } = useTranslation();
  const [val, setVal] = useState('');
  const add = async (e) => {
    e.preventDefault();
    const trimmed = val.trim();
    if (!trimmed) return;
    if (await save({ custom_urls: [...urls, trimmed] })) setVal('');
  };
  return (
    <div className="field">
      <b>{t('conn_cfg.custom_title')}</b>
      <span className="field-hint">{t('conn_cfg.custom_hint')}</span>
      {urls.length > 0 && (
        <div className="chips">
          {urls.map((u) => {
            const href = u.startsWith('http://') || u.startsWith('https://') ? `${u}/` : null;
            return (
              <span key={u} className="chip chip-x mono">
                {href ? (
                  <a href={href} target="_blank" rel="noopener noreferrer">
                    {u}
                  </a>
                ) : (
                  u
                )}
                <CopyButton text={href || u} label={false} variant="plain" />
                <button
                  type="button"
                  aria-label={t('conn_cfg.custom_remove', { url: u })}
                  disabled={busy}
                  onClick={() => save({ custom_urls: urls.filter((x) => x !== u) })}
                >
                  <X size={14} />
                </button>
              </span>
            );
          })}
        </div>
      )}
      <form className="row" onSubmit={add}>
        <input
          value={val}
          onChange={(e) => {
            setVal(e.target.value);
            if (error) clearError();
          }}
          className="mono"
          inputMode="url"
          autoComplete="off"
          autoCapitalize="off"
          spellCheck={false}
          placeholder={t('conn_cfg.custom_ph')}
          aria-label={t('conn_cfg.custom_title')}
          aria-invalid={error ? 'true' : undefined}
        />
        <Button type="submit" variant="secondary" icon={Plus} busy={busy} disabled={!val.trim()}>
          {t('access.list_add')}
        </Button>
      </form>
      {error && <span className="field-error">{t(`error:rule.${error.params?.rule}`, { defaultValue: errorText(t, error) })}</span>}
    </div>
  );
}

function CustomTlsForm({ certPath, keyPath, save, busy, error, clearError }) {
  const { t } = useTranslation();
  const [cp, setCp] = useState(certPath || '');
  const [kp, setKp] = useState(keyPath || '');
  useEffect(() => {
    setCp(certPath || '');
    setKp(keyPath || '');
  }, [certPath, keyPath]);
  const changed = cp.trim() !== (certPath || '') || kp.trim() !== (keyPath || '');
  const hasSaved = Boolean(certPath || keyPath);
  return (
    <form
      className="field"
      onSubmit={(e) => {
        e.preventDefault();
        save({ tls_cert_path: cp.trim(), tls_key_path: kp.trim() });
      }}
    >
      <b>{t('conn_cfg.cert_title')}</b>
      <span className="field-hint">{t('conn_cfg.cert_hint')}</span>
      <Field label={t('conn_cfg.cert_file')}>
        <input
          value={cp}
          onChange={(e) => {
            setCp(e.target.value);
            if (error) clearError();
          }}
          className="mono"
          placeholder={t('conn_cfg.cert_file_ph')}
          autoComplete="off"
          spellCheck={false}
        />
      </Field>
      <Field label={t('conn_cfg.key_file')}>
        <input
          value={kp}
          onChange={(e) => {
            setKp(e.target.value);
            if (error) clearError();
          }}
          className="mono"
          placeholder={t('conn_cfg.key_file_ph')}
          autoComplete="off"
          spellCheck={false}
        />
      </Field>
      {error && <span className="field-error">{t(`error:rule.${error.params?.rule}`, { defaultValue: errorText(t, error) })}</span>}
      <div className="row" style={{ marginTop: 8 }}>
        <Button type="submit" busy={busy} disabled={!changed || Boolean(cp.trim()) !== Boolean(kp.trim())}>
          {t('action.save')}
        </Button>
        {hasSaved && (
          <Button
            variant="ghost"
            busy={busy}
            onClick={() => {
              if (error) clearError();
              save({ tls_cert_path: '', tls_key_path: '' });
            }}
          >
            {t('conn_cfg.cert_clear')}
          </Button>
        )}
      </div>
    </form>
  );
}

function Probes({ items }) {
  const { t } = useTranslation();
  if (!items) return null;
  const ips = items.filter((p) => p.ip).map((p) => p.ip);
  return (
    <span className="field-hint direct-probes">
      {items.map((p) => (
        <span key={p.source} className="direct-probe">
          <span>{p.source === 'relay' ? t('direct.source_relay') : p.source}</span>
          <span className="mono">{p.ip || t('direct.probe_failed', { why: p.error })}</span>
        </span>
      ))}
      {ips.some((ip) => ip !== ips[0]) && <span className="direct-probe-warn">{t('direct.probe_disagree')}</span>}
    </span>
  );
}

const NAT_KIND = { open: 'ok', punch: 'ok', partial: 'warn', relay_only: 'info', wrong_addr: 'warn', proxy: 'info' };

function NatResult({ r, ports, filled, onFill }) {
  const { t } = useTranslation();
  if (!r) return null;
  const key = r.verdict === 'unknown' || r.verdict === 'wrong_addr' ? `${r.verdict}_${r.why}` : r.verdict;
  const vars = { exit: r.exit, ip: r.public_v4, begin: ports[0], end: ports[1] };
  return (
    <>
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
          {(!r.a || r.a === 'skipped') && (
            <span className="direct-probe">
              <span>{t('direct.nat.from_outside')}</span>
              <span>
                {t('direct.nat.a_skipped')}
                {r.a_why ? ` · ${t(`direct.nat.a_why_${r.a_why}`, { defaultValue: r.a_why })}` : ''}
              </span>
            </span>
          )}
          <span className="direct-probe">
            <span>{t('direct.nat.local_port')}</span>
            <span className="mono">
              {r.local_port || '—'}
              {!r.in_range && r.local_port ? ` · ${t('direct.nat.random_port')}` : ''}
            </span>
          </span>
          {(r.mappings ?? []).map((m) => (
            <span key={m.relay} className="direct-probe">
              <span>{m.relay}</span>
              <span className="mono">{m.mapped || t('direct.probe_failed', { why: m.error })}</span>
            </span>
          ))}
        </span>
      </FoldNote>
      {r.suggest && !filled && (
        <span className="field-hint" style={{ display: 'block' }}>
          <button type="button" className="link-button" onClick={() => onFill(r.suggest)}>
            {t('direct.nat.suggest', { ip: r.suggest })}
          </button>{' '}
          {t('direct.nat.suggest_hint')}
        </span>
      )}
    </>
  );
}

function NetAdvanced({ net, clashIp, onSaved }) {
  const { t } = useTranslation();
  const toast = useToast();
  const [f, setF] = useState(() => netForm(net));
  const [detected, setDetected] = useState(null);
  const { busy, error, run } = useAction();
  const detect = useAction();

  useEffect(() => {
    setF(netForm(net));
  }, [net.port_begin, net.port_end, net.public_v4, net.public_v6]);

  const save = async (body) => {
    const r = await run(() => api.put('/rtc/net', body));
    if (r) {
      setF(netForm(r));
      toast(t('common.saved'));
      onSaved(r);
    }
  };

  const set = (k) => (e) => setF({ ...f, [k]: e.target.value });
  const base = netForm(net);
  const changed = Object.keys(f).some((k) => f[k] !== base[k]);
  const [defBegin, defEnd] = net.default_ports ?? [50000, 50031];
  const isDefault = f.port_begin === String(defBegin) && f.port_end === String(defEnd);

  const addrHint = (k, fallback) => {
    const r = net[`resolved_${k}`];
    if (r === undefined || f[`public_${k}`] !== (net[`public_${k}`] || '')) return fallback;
    return r ? t('direct.resolved', { ip: r }) : t('direct.unresolved');
  };
  const fieldError = (name) => (error?.params?.field === name ? t(`error:rule.${error.params.rule}`) : undefined);

  const runDetect = async () => {
    const r = await detect.run(() => api.post('/rtc/net/detect', {}));
    if (!r) return;
    setDetected(r);
    if (r.nat) onSaved({ ...net, nat: r.nat });
    const pick = (cur, ip) => (isName(cur) ? cur : ip || cur);
    setF((cur) => ({ ...cur, public_v4: pick(cur.public_v4, r.suggest_v4), public_v6: pick(cur.public_v6, r.suggest_v6) }));
  };

  const clashRule = clashIp && !clashIp.includes(':') ? `- IP-CIDR,${clashIp}/32,${t('public_https.clash_node')},no-resolve` : '';

  return (
    <div className="field" style={{ marginTop: 12 }}>
      <b>{t('conn_cfg.net_title')}</b>
      <NatResult r={net.nat} ports={[net.port_begin, net.port_end]} filled={Boolean(net.public_v4)} onFill={(ip) => save({ public_v4: ip })} />
      <form
        onSubmit={(e) => {
          e.preventDefault();
          save({
            port_begin: Number(f.port_begin),
            port_end: Number(f.port_end),
            public_v4: f.public_v4.trim(),
            public_v6: f.public_v6.trim(),
          });
        }}
      >
        <Field label={t('direct.public_v4')} hint={addrHint('v4', t('direct.public_v4_hint'))} error={fieldError('public_v4')}>
          <input value={f.public_v4} onChange={set('public_v4')} className="mono" placeholder={t('direct.not_set')} autoComplete="off" autoCapitalize="off" spellCheck={false} />
        </Field>
        {detected && <Probes items={detected.v4} />}
        <Field
          label={t('direct.public_v6')}
          hint={addrHint('v6', net.nic_v6?.length ? t('direct.public_v6_nic', { ips: net.nic_v6.join(', ') }) : t('direct.public_v6_hint'))}
          error={fieldError('public_v6')}
        >
          <input value={f.public_v6} onChange={set('public_v6')} className="mono" placeholder={t('direct.not_set')} autoComplete="off" autoCapitalize="off" spellCheck={false} />
        </Field>
        {detected && <Probes items={detected.v6} />}
        <Field label={t('direct.ports')} hint={t('direct.ports_hint')} error={fieldError('rtc_ports')}>
          <span className="row direct-ports">
            <input value={f.port_begin} onChange={set('port_begin')} inputMode="numeric" className="mono" aria-label={t('direct.port_begin')} />
            <span aria-hidden="true">–</span>
            <input value={f.port_end} onChange={set('port_end')} inputMode="numeric" className="mono" aria-label={t('direct.port_end')} />
            {!isDefault && (
              <Button variant="plain" onClick={() => setF({ ...f, port_begin: String(defBegin), port_end: String(defEnd) })}>
                {t('direct.ports_default')}
              </Button>
            )}
          </span>
        </Field>
        <div className="row" style={{ margin: '12px 0' }}>
          <Button type="submit" busy={busy} disabled={!changed}>
            {t('action.save')}
          </Button>
          <Button variant="secondary" icon={Search} busy={detect.busy} onClick={runDetect}>
            {t('direct.detect')}
          </Button>
        </div>
        <span className="field-hint">{t('direct.detect_hint')}</span>
        <ErrorNote error={detect.error} />
      </form>
      <Row label={t('direct.ipv6')} hint={t('direct.ipv6_hint')}>
        <Switch checked={net.ipv6} onChange={(v) => save({ ipv6: v })} disabled={busy} label={t('direct.ipv6')} />
      </Row>
      <Note>{t('direct.firewall', { begin: net.port_begin, end: net.port_end })}</Note>
      {clashRule && (
        <span className="field-hint" style={{ display: 'block', marginTop: 8 }}>
          {t('public_https.clash')}
          <Snippet text={clashRule} />
        </span>
      )}
      <ErrorNote error={error?.params?.field ? null : error} />
    </div>
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

  const saveConn = async (body) => {
    const r = await connAct.run(() => api.put('/connection', body));
    if (r) {
      setData(r);
      toast(t('common.saved'));
      reload();
    }
    return Boolean(r);
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

  const st = data.direct_status || {};
  const net = data.net || { port_begin: 50000, port_end: 50031, default_ports: [50000, 50031], public_v4: '', public_v6: '', ipv6: true, nic_v6: [] };
  const [defBegin, defEnd] = net.default_ports ?? [50000, 50031];
  const advOpen = Boolean(data.tls_cert_path || data.tls_key_path || net.public_v4 || net.public_v6 || net.port_begin !== defBegin || net.port_end !== defEnd);

  const lanUrls = (data.lan_ips ?? []).map((ip) => `http://${urlHost(ip)}:${data.port}/`);
  const pubEndpoints = (data.endpoints ?? []).filter((e) => e.kind !== 'lan' && e.kind !== 'tunnel');
  const urlError = connAct.error?.params?.field === 'custom_urls' ? connAct.error : null;
  const certError = connAct.error?.params?.field === 'tls_cert' ? connAct.error : null;
  const listError = accessAct.error?.params?.field === 'allowed_ips' ? accessAct.error : null;
  const clashIp = st.ip || net.resolved_v4 || (!isName(net.public_v4 || '') && net.public_v4) || null;

  return (
    <>
      <div id="public-https" className="anchor">
        <Card title={t('conn_cfg.title')} sub={t('conn_cfg.intro')}>
          <Row label={t('conn_cfg.lan')} hint={t('conn_cfg.lan_hint')}>
            <Switch checked={data.allow_lan} onChange={(v) => saveConn({ allow_lan: v })} disabled={connAct.busy} label={t('conn_cfg.lan')} />
          </Row>
          {data.allow_lan && (
            <div className="access-sub">
              {lanUrls.length > 0 ? (
                <>
                  <span className="field-hint">{t('access.lan_urls')}</span>
                  {lanUrls.map((u) => (
                    <span key={u} className="one-line">
                      <a className="mono ellipsis" href={u} target="_blank" rel="noopener noreferrer">
                        {u}
                      </a>
                      <CopyButton text={u} label={false} variant="plain" />
                    </span>
                  ))}
                </>
              ) : (
                <span className="field-hint">{t('access.lan_unknown', { port: data.port })}</span>
              )}
            </div>
          )}
          {status.capabilities.os === 'windows' && data.allow_lan && <WindowsFirewall exe={data.exe} />}

          <Row label={t('conn_cfg.pub')} hint={t('conn_cfg.pub_hint')}>
            <Switch checked={data.public_direct} onChange={(v) => saveConn({ public_direct: v })} disabled={connAct.busy} label={t('conn_cfg.pub')} />
          </Row>
          {data.public_direct && (
            <div className="access-sub">
              {pubEndpoints.length > 0 && (
                <>
                  <span className="field-hint">{t('conn_cfg.direct_urls')}</span>
                  {pubEndpoints.map((e) => (
                    <span key={e.url} className="one-line">
                      <Globe size={14} aria-hidden="true" />
                      {e.web_tls ? (
                        <a className="mono ellipsis" href={`${e.url}/`} target="_blank" rel="noopener noreferrer">
                          {e.url}
                        </a>
                      ) : (
                        <span className="mono ellipsis">{e.url}</span>
                      )}
                      <span className={`badge ${e.web_tls ? 'badge-ok' : ''}`}>{t(e.web_tls ? 'conn_cfg.tag_web' : 'conn_cfg.tag_app')}</span>
                      <CopyButton text={e.web_tls ? `${e.url}/` : e.url} label={false} variant="plain" />
                    </span>
                  ))}
                </>
              )}
              {st.phase === 'working' && <Note>{t('public_https.working', { ip: st.ip || '' })}</Note>}
              {st.phase === 'ok' && (
                <>
                  <span className="field-hint">
                    {t('public_https.cert_until')} <Time value={iso(st.not_after)} />
                    {st.renew_at && (
                      <>
                        {' · '}
                        {t('public_https.renew_at')} <Time value={iso(st.renew_at)} only="relative" />
                      </>
                    )}
                    {st.method ? ` · ${t(`public_https.method.${st.method}`)}` : ''}
                  </span>
                  {st.reach && st.reach !== 'ok' && st.reach !== 'unknown' && (
                    <Note kind="info">{t('public_https.unreachable', { port: data.port, why: st.reach })}</Note>
                  )}
                </>
              )}
              {st.phase !== 'working' && st.phase !== 'ok' && (
                <FoldNote kind="info" summary={t('conn_cfg.browser_https_fold')}>
                  <p className="small">{t('conn_cfg.browser_https_body', { port: data.port, tos: data.tos_url })}</p>
                  <Failure st={st} port={data.port} />
                </FoldNote>
              )}
            </div>
          )}

          <CustomUrls
            urls={data.custom_urls ?? []}
            save={saveConn}
            busy={connAct.busy}
            error={urlError}
            clearError={() => connAct.setError(null)}
          />

          <More summary={t('conn_cfg.advanced')} open={advOpen}>
            <CustomTlsForm
              certPath={data.tls_cert_path}
              keyPath={data.tls_key_path}
              save={saveConn}
              busy={connAct.busy}
              error={certError}
              clearError={() => connAct.setError(null)}
            />
            <NetAdvanced net={net} clashIp={clashIp} onSaved={onSavedNet} />
          </More>
          <ErrorNote error={urlError || certError ? null : connAct.error} />
        </Card>
      </div>

      <div id="access" className="anchor">
        <Card title={t('conn_cfg.sec_title')} sub={t('conn_cfg.sec_intro')}>
          <CodeBox data={data} save={saveAccess} busy={accessAct.busy} />
          {!data.code_ok && (data.public_direct || (data.custom_urls ?? []).length > 0) && (
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
    </>
  );
}
