// 插件管理（工具 → 插件）：这台电脑上装的插件、服务端目录里这个平台能装的插件和这个账号的授权。
// 授权只能由我们在管理后台手动开通（带到期时间），这里只显示、不能自己开通。装 / 更新在电脑后台进行，界面轮询进度。
import { Download, ExternalLink, Puzzle, RefreshCw, RotateCw, Trash2 } from 'lucide-react';
import { useEffect } from 'react';
import { useTranslation } from 'react-i18next';

import { useStatus } from '../App.jsx';
import { api } from '../shared/api.js';
import { errorText } from '../shared/i18n.js';
import { PageHead } from '../shared/page.jsx';
import { pickLocalized, pluginIcon, pluginRoute } from '../shared/plugins.jsx';
import { useRouter } from '../shared/router.jsx';
import { formatTime } from '../shared/time.js';
import { Button, Card, Empty, ErrorNote, Note, Skeleton, useAction, useConfirm, useLoad, useToast } from '../shared/ui.jsx';

const mb = (n) => (n ? `${(n / 1048576).toFixed(n >= 100 * 1048576 ? 0 : 1)} MB` : '');

function License({ p }) {
  const { t } = useTranslation();
  if (p.licensed == null) return null;
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
      {data && !data.signed_in && <Note>{t('plugins.sign_in')}</Note>}
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
    </>
  );
}
