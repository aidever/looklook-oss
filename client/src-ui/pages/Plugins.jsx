// 插件管理（工具 → 插件）：这台电脑上装的插件、服务端目录里这个平台能装的插件和这个账号的授权。
// 授权只能由我们在管理后台手动开通（带到期时间），这里只显示、不能自己开通。装 / 更新在电脑后台进行，界面轮询进度。
// 不能联网或不登录的电脑：复制本机机器指纹找我们要离线授权码，粘贴到这里；插件包用离线安装包（.llplugin）从文件装。
import { Download, ExternalLink, FileUp, KeyRound, Puzzle, RefreshCw, RotateCw, Trash2 } from 'lucide-react';
import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';

import { useStatus } from '../App.jsx';
import { api, deviceApi, selectedDevice } from '../shared/api.js';
import { errorText } from '../shared/i18n.js';
import { PageHead } from '../shared/page.jsx';
import { pickLocalized, pluginIcon, pluginRoute } from '../shared/plugins.jsx';
import { useRouter } from '../shared/router.jsx';
import { formatTime } from '../shared/time.js';
import { Button, Card, CopyButton, Empty, ErrorNote, Field, Note, Skeleton, useAction, useConfirm, useLoad, useToast } from '../shared/ui.jsx';
import { dirTransport, runUpload } from '../shared/uploader.js';

const mb = (n) => (n ? `${(n / 1048576).toFixed(n >= 100 * 1048576 ? 0 : 1)} MB` : '');

function License({ p }) {
  const { t } = useTranslation();
  if (p.licensed == null) return null;
  if (p.offline_until) return <span className="small muted">{t('plugins.offline_until', { time: formatTime(p.offline_until) })}</span>;
  if (p.licensed) return <span className="small muted">{p.expires_at ? t('plugins.licensed_until', { time: formatTime(p.expires_at) }) : t('plugins.licensed')}</span>;
  return <span className="small" style={{ color: "var(--warn)" }}>{p.expires_at ? t('plugins.license_expired', { time: formatTime(p.expires_at) }) : t('plugins.not_licensed')}</span>;
}

function Job({ job }) {
  const { t } = useTranslation();
  if (!job || job.step === 'done') return null;
  if (job.step === 'failed') return <ErrorNote error={job.error} />;
  const pct = job.total ? Math.min(100, Math.round((job.done / job.total) * 100)) : null;
  return (
    <div className="stack" style={{ gap: 4 }}>
      <span className="small muted">
        {t(`plugins.step_${job.step}`)}
        {job.step === 'download' && pct != null ? ` ${pct}%` : ''}
      </span>
      {job.step === 'download' && pct != null && <progress max={100} value={pct} style={{ width: '100%' }} />}
    </div>
  );
}

function PluginCard({ p, onChanged }) {
  const { t, i18n } = useTranslation();
  const { navigate } = useRouter();
  const { busy, error, run } = useAction();
  const confirm = useConfirm();
  const toast = useToast();
  const Icon = pluginIcon(p.icon);
  const title = pickLocalized(p.title, i18n.language) || p.name;
  const desc = pickLocalized(p.description, i18n.language);
  const working = p.job && !['done', 'failed'].includes(p.job.step);
  const install = () =>
    run(async () => {
      await api.post(`/plugin-store/${p.name}/install`);
      onChanged();
    });
  const uninstall = async () => {
    if (!(await confirm({ title: t('plugins.uninstall_title', { name: title }), body: t('plugins.uninstall_body'), confirm: t('plugins.uninstall'), danger: true }))) return;
    run(async () => {
      await api.del(`/plugin-store/${p.name}`);
      toast(t('plugins.uninstalled'));
      onChanged();
    });
  };
  const restart = () =>
    run(async () => {
      await api.post(`/plugin-store/${p.name}/restart`);
      toast(t('plugins.restarted'));
      onChanged();
    });
  return (
    <Card>
      <div className="row" style={{ alignItems: 'flex-start', gap: 12 }}>
        <Icon size={28} aria-hidden="true" style={{ flex: 'none', marginTop: 2 }} />
        <div className="grow stack" style={{ gap: 6 }}>
          <div className="between">
            <b>{title}</b>
            <span className="small muted">
              {p.installed ? t('plugins.version_installed', { v: p.version }) : p.latest ? t('plugins.version_available', { v: p.latest }) : ''}
              {p.running ? ` · ${t('plugins.running')}` : ''}
            </span>
          </div>
          {desc && <span className="small muted">{desc}</span>}
          <License p={p} />
          {p.broken && <Note kind="warn">{t('plugins.broken')}</Note>}
          {p.incompatible && <Note kind="warn">{errorText(t, p.incompatible)}</Note>}
          <Job job={p.job} />
          <ErrorNote error={error} />
          <div className="inline">
            {p.installed && !p.incompatible && (
              <Button icon={ExternalLink} onClick={() => navigate(pluginRoute(p.name))}>
                {t('plugins.open')}
              </Button>
            )}
            {(!p.installed || p.update || p.broken) && p.licensed && (
              <Button icon={Download} variant={p.installed ? 'secondary' : 'primary'} busy={busy || working} onClick={install}>
                {p.installed && !p.broken ? t('plugins.update_to', { v: p.latest }) : t('plugins.install', { size: mb(p.size) })}
              </Button>
            )}
            {p.installed && (
              <Button icon={RotateCw} variant="ghost" busy={busy} onClick={restart}>
                {t('plugins.restart')}
              </Button>
            )}
            {(p.installed || p.broken) && (
              <Button icon={Trash2} variant="ghost" busy={busy} onClick={uninstall}>
                {t('plugins.uninstall')}
              </Button>
            )}
          </div>
        </div>
      </div>
    </Card>
  );
}

/** 指纹每 4 位隔一个空格，好念好对 */
const groupFp = (m) => (m ?? '').replace(/(.{4})(?=.)/g, '$1 ');

/** 平台：下载给别的电脑用的离线安装包时选 */
const PLATFORMS = ['linux-x86_64', 'linux-aarch64', 'darwin-aarch64', 'darwin-x86_64', 'windows-x86_64'];

/** 离线授权：本机机器指纹、填授权码、已填的码；从文件安装；（登录时）下载离线安装包 */
function Offline({ data, onChanged }) {
  const { t, i18n } = useTranslation();
  const toast = useToast();
  const confirm = useConfirm();
  const { busy, error, run } = useAction();
  const [code, setCode] = useState('');
  const [upload, setUpload] = useState(null);
  const fileRef = useRef(null);
  const installed = data.items.filter((p) => p.licensed || p.installed);
  const [dl, setDl] = useState({ name: '', platform: data.platform });
  const save = () =>
    run(async () => {
      const r = await api.post('/plugin-store/offline', { code });
      setCode('');
      toast(t('plugins.offline_saved', { names: (r.added ?? []).join(', ') }));
      onChanged();
    });
  const remove = async (o) => {
    if (!(await confirm({ title: t('plugins.offline_remove_title', { name: o.plugin }), body: t('plugins.offline_remove_body'), confirm: t('plugins.offline_remove'), danger: true }))) return;
    run(async () => {
      await api.del(`/plugin-store/offline/${o.plugin}`);
      onChanged();
    });
  };
  const pick = (e) => {
    const file = e.target.files?.[0];
    e.target.value = '';
    if (!file) return;
    run(async () => {
      const task = { file, dest: data.incoming_dir, sent: 0 };
      setUpload({ ...task });
      try {
        const path = await runUpload(task, { transport: dirTransport(selectedDevice() ?? undefined), onChange: () => setUpload({ ...task }) });
        setUpload({ ...task, installing: true });
        const r = await api.post('/plugin-store/install-file', { path });
        const p = r.items?.find((x) => x.name === r.installed_name);
        toast(t('plugins.file_installed', { name: pickLocalized(p?.title, i18n.language) || r.installed_name }));
        onChanged();
      } finally {
        setUpload(null);
      }
    });
  };
  const download = () =>
    run(async () => {
      const name = dl.name || installed[0]?.name;
      const r = await deviceApi(selectedDevice() ?? undefined).get(`/plugin-store/${name}/offline-package?platform=${encodeURIComponent(dl.platform)}`);
      window.open(r.url, '_blank', 'noopener');
    });
  const pct = upload?.file?.size ? Math.min(100, Math.round(((upload.sent ?? 0) / upload.file.size) * 100)) : 0;
  return (
    <Card title={t('plugins.offline_title')} sub={t('plugins.offline_sub')}>
      <div className="stack">
        <Field label={t('plugins.fingerprint')} hint={t('plugins.fingerprint_hint')}>
          <div className="inline" style={{ flexWrap: 'wrap' }}>
            <code className="mono" style={{ wordBreak: 'break-all', userSelect: 'all' }}>{data.machine ? groupFp(data.machine) : '—'}</code>
            {data.machine && <CopyButton text={data.machine} />}
          </div>
        </Field>
        {data.offline?.length > 0 && (
          <div className="stack" style={{ gap: 6 }}>
            {data.offline.map((o) => (
              <div key={o.plugin} className="between small">
                <span>
                  <span className="mono">{o.plugin}</span>
                  {' · '}
                  <span className={o.state === 'valid' ? 'muted' : ''} style={o.state === 'valid' ? undefined : { color: 'var(--warn)' }}>
                    {t(`plugins.offline_state_${o.state}`, { time: o.expires_at ? formatTime(o.expires_at) : '' })}
                  </span>
                </span>
                <Button variant="ghost" icon={Trash2} busy={busy} onClick={() => remove(o)} aria-label={t('plugins.offline_remove')} />
              </div>
            ))}
          </div>
        )}
        <Field label={t('plugins.offline_code')}>
          <textarea
            className="mono small"
            rows={3}
            value={code}
            onChange={(e) => setCode(e.target.value)}
            placeholder="LL1L.…"
            spellCheck={false}
            autoCapitalize="off"
            autoCorrect="off"
            style={{ width: '100%', wordBreak: 'break-all' }}
          />
        </Field>
        <ErrorNote error={error} />
        <div className="inline" style={{ flexWrap: 'wrap' }}>
          <Button icon={KeyRound} busy={busy} disabled={!code.trim()} onClick={save}>
            {t('plugins.offline_save')}
          </Button>
          <Button icon={FileUp} variant="secondary" busy={busy} disabled={!data.keys} onClick={() => fileRef.current?.click()}>
            {t('plugins.install_file')}
          </Button>
          <input ref={fileRef} type="file" accept=".llplugin" hidden onChange={pick} />
        </div>
        {upload && (
          <div className="stack" style={{ gap: 4 }}>
            <span className="small muted">{upload.installing ? t('plugins.step_install') : t('plugins.uploading', { pct })}</span>
            {!upload.installing && <progress max={100} value={pct} style={{ width: '100%' }} />}
          </div>
        )}
        {data.signed_in && installed.length > 0 && (
          <div className="stack" style={{ gap: 6 }}>
            <span className="small muted">{t('plugins.offline_package_hint')}</span>
            <div className="inline" style={{ flexWrap: 'wrap' }}>
              <select value={dl.name || installed[0].name} onChange={(e) => setDl({ ...dl, name: e.target.value })} aria-label={t('plugins.offline_package_plugin')}>
                {installed.map((p) => (
                  <option key={p.name} value={p.name}>
                    {pickLocalized(p.title, i18n.language) || p.name}
                  </option>
                ))}
              </select>
              <select value={dl.platform} onChange={(e) => setDl({ ...dl, platform: e.target.value })} aria-label={t('plugins.offline_package_platform')}>
                {[...new Set([data.platform, ...PLATFORMS])].map((x) => (
                  <option key={x} value={x}>
                    {x}
                  </option>
                ))}
              </select>
              <Button icon={Download} variant="ghost" busy={busy} onClick={download}>
                {t('plugins.offline_package')}
              </Button>
            </div>
          </div>
        )}
      </div>
    </Card>
  );
}

export default function Plugins() {
  const { t } = useTranslation();
  const { reload: reloadStatus } = useStatus();
  const [data, error, reload, loading] = useLoad(() => api.get('/plugin-store'), []);
  const working = (data?.items ?? []).some((p) => p.job && !['done', 'failed'].includes(p.job.step));
  // 装着的时候每秒看一次进度
  useEffect(() => {
    if (!working) return undefined;
    const id = setTimeout(reload, 1000);
    return () => clearTimeout(id);
  }, [data, working]);
  const changed = () => {
    reload();
    reloadStatus?.();
  };
  return (
    <>
      <PageHead title={t('plugins.title')} icon={Puzzle} sub={t('plugins.sub')} perDevice>
        <Button variant="ghost" icon={RefreshCw} busy={loading} onClick={reload} aria-label={t('plugins.refresh')} />
      </PageHead>
      <ErrorNote error={error} />
      {data && !data.signed_in && <Note>{t('plugins.sign_in_or_offline')}</Note>}
      {data?.catalog_error && <ErrorNote error={data.catalog_error} />}
      {data && !data.keys && <Note kind="warn">{t('plugins.no_keys')}</Note>}
      {!data && !error && <Skeleton height={96} />}
      {data && data.items.length === 0 && (
        <Empty icon={Puzzle} title={t('plugins.empty')}>
          <span className="small muted">{t('plugins.empty_hint', { platform: data.platform })}</span>
        </Empty>
      )}
      {data?.items.map((p) => <PluginCard key={p.name} p={p} onChanged={changed} />)}
      {data?.items.some((p) => p.licensed === false) && <p className="small muted">{t('plugins.license_hint')}</p>}
      {data && <Offline data={data} onChanged={changed} />}
    </>
  );
}
