// 设置 → 网络与访问 → 公网 HTTPS 直连（服务端 docs/09）：有固定公网 IPv4 的电脑自己向 Let's Encrypt 申请这个 IP 的证书，
// 管理台在原端口上同时提供 HTTPS。浏览器直接打开 https://IP:端口，不经过平台和中转；想加速就在 Clash 里给这个 IP 加一条规则。
// 默认关闭；公网来源必须认证（平台的“直接打开”或至少 12 位的访问码）。本机、局域网和远程打开均可查看和修改。
import { Globe } from 'lucide-react';
import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';

import { api } from '../shared/api.js';
import { Row } from '../shared/page.jsx';
import { Card, CopyButton, ErrorNote, Note, Skeleton, Switch, Time, useAction, useConfirm, useToast } from '../shared/ui.jsx';

const iso = (s) => (s ? new Date(s * 1000).toISOString() : null);

/** 一段可以复制的配置。 */
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

/** 申请失败的原因和怎么改。 */
function Failure({ st, port }) {
  const { t } = useTranslation();
  const code = st.error;
  if (!code) return null;
  const nginx = `location /.well-known/acme-challenge/ {\n    proxy_pass http://127.0.0.1:${port};\n}`;
  const caddy = `handle /.well-known/acme-challenge/* {\n    reverse_proxy 127.0.0.1:${port}\n}`;
  return (
    <Note kind={st.phase === 'ok' ? 'info' : 'warn'}>
      <b>{t(`public_https.err.${code}`, { defaultValue: t('public_https.err.other', { code }) })}</b>
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
      {st.detail && <span className="field-hint mono" style={{ display: 'block' }}>{st.detail}</span>}
      {st.next_try && (
        <span className="field-hint" style={{ display: 'block' }}>
          {t('public_https.next_try')} <Time value={iso(st.next_try)} only="relative" />
        </span>
      )}
    </Note>
  );
}

export default function PublicHttpsCard() {
  const { t } = useTranslation();
  const toast = useToast();
  const confirm = useConfirm();
  const [data, setData] = useState(null);
  const [loadError, setLoadError] = useState(null);
  const { busy, error, run } = useAction();
  const load = () => api.get('/public-https').then(setData, setLoadError);
  useEffect(() => {
    load();
  }, []);
  // 申请中隔几秒刷新一次状态
  const phase = data?.status?.phase;
  useEffect(() => {
    if (phase !== 'working') return undefined;
    const id = setInterval(load, 3000);
    return () => clearInterval(id);
  }, [phase]);

  if (loadError) return <ErrorNote error={loadError} />;
  if (!data) return <Skeleton height={160} />;

  const toggle = async (on) => {
    if (on && !(await confirm({ title: t('public_https.confirm_title'), body: t('public_https.confirm_body', { port: data.port, tos: data.tos_url }), confirm: t('public_https.confirm_ok') }))) return;
    const r = await run(() => api.put('/public-https', { enabled: on }));
    if (r) {
      setData(r);
      toast(t('common.saved'));
    }
  };
  const st = data.status || {};
  const ip = st.ip || null;
  const rule = ip ? `- IP-CIDR,${ip}/32,${t('public_https.clash_node')},no-resolve` : '';
  return (
    <Card title={t('public_https.title')} sub={t('public_https.intro')}>
      <Row label={t('public_https.enable')} hint={t('public_https.enable_hint')}>
        <Switch checked={data.enabled} onChange={toggle} disabled={busy} label={t('public_https.enable')} />
      </Row>
      {data.enabled && (
        <div className="access-sub">
          {st.phase === 'no_ip' && <Note kind="warn">{t('public_https.no_ip')}</Note>}
          {st.phase === 'working' && <Note>{t('public_https.working', { ip })}</Note>}
          {st.phase === 'ok' && st.url && (
            <>
              <span className="field-hint">{t('public_https.url')}</span>
              <span className="one-line">
                <Globe size={14} />
                <a className="mono ellipsis" href={`${st.url}/`} target="_blank" rel="noopener noreferrer">
                  {st.url}
                </a>
                <CopyButton text={`${st.url}/`} label={false} variant="plain" />
              </span>
              <span className="field-hint">
                {t('public_https.cert_until')} <Time value={iso(st.not_after)} /> · {t('public_https.renew_at')} <Time value={iso(st.renew_at)} only="relative" />
                {st.method ? ` · ${t(`public_https.method.${st.method}`)}` : ''}
              </span>
              {st.reach && st.reach !== 'ok' && st.reach !== 'unknown' && <Note kind="warn">{t('public_https.unreachable', { port: data.port, why: st.reach })}</Note>}
            </>
          )}
          <Failure st={st} port={data.port} />
          {!data.code_ok && <Note kind="warn">{t('public_https.code_short')}</Note>}
          {data.code_ok && !data.logged_in && st.phase === 'ok' && <Note>{t('public_https.code_login', { code: data.code })}</Note>}
          <Note>{t('public_https.firewall', { port: data.port })}</Note>
          {ip && (
            <span className="field-hint" style={{ display: 'block' }}>
              {t('public_https.clash')}
              <Snippet text={rule} />
            </span>
          )}
          {data.logins.length > 0 && (
            <div className="field">
              <b>{t('public_https.logins')}</b>
              {data.logins.map((l) => (
                <span key={`${l.ip}-${l.at}`} className="field-hint mono" style={{ display: 'block' }}>
                  {l.ip} · {t(`public_https.via.${l.via}`)} · <Time value={new Date(l.at).toISOString()} only="relative" />
                </span>
              ))}
            </div>
          )}
        </div>
      )}
      <ErrorNote error={error} />
    </Card>
  );
}
