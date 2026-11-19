// 通用插件管理页（/plugins）：从服务端拉取当前平台可用的插件目录、授权情况与安装/更新/启停/卸载控制。
import { ArrowUpRight, Check, Download, Lock, Play, Puzzle, RefreshCw, Square, Trash2 } from 'lucide-react';
import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';

import { api } from '../shared/api.js';
import { useDevices } from '../shared/devices.jsx';
import { errorText } from '../shared/i18n.js';
import { pickLocalized, pluginIcon, pluginRoute } from '../shared/plugins.jsx';
import { useRouter } from '../shared/router.jsx';
import { useStatus } from '../App.jsx';
import { Button, Card, Empty, ErrorNote, Note, Skeleton, useAction, useConfirm, useToast } from '../shared/ui.jsx';

const POLL_MS = 1200;

function fmtMB(n) {
  if (!n) return '';
  if (n >= 1 << 20) return `${(n / (1 << 20)).toFixed(1)} MB`;
  return `${Math.max(1, Math.round(n / 1024))} KB`;
}

export default function Plugins() {
  const { t, i18n } = useTranslation();
  const { navigate } = useRouter();
  const { status, reload: reloadStatus } = useStatus();
  const devices = useDevices();
  const toast = useToast();
  const confirm = useConfirm();
  const { busy, run } = useAction();
  const [data, setData] = useState(null);
  const [error, setError] = useState(null);

  const load = (withCatalog = true) =>
    api.get(withCatalog ? '/plugins?catalog=1' : '/plugins').then(
      (d) => {
        setData((prev) => ({
          ...d,
          catalog: d.catalog ?? prev?.catalog ?? null,
          catalog_error: withCatalog ? d.catalog_error : prev?.catalog_error ?? null,
        }));
        setError(null);
        return d;
      },
      (e) => setError(e),
    );

  useEffect(() => {
    load(true);
  }, [devices.selected]);

  const anyRunningJob = Object.values(data?.jobs ?? {}).some((j) => j?.state === 'running');
  useEffect(() => {
    if (!anyRunningJob) return undefined;
    const id = setInterval(() => {
      if (document.visibilityState === 'visible') {
        load(false).then((d) => {
          if (d && !Object.values(d.jobs ?? {}).some((j) => j?.state === 'running')) {
            reloadStatus();
            load(true);
          }
        });
      }
    }, POLL_MS);
    return () => clearInterval(id);
  }, [anyRunningJob]);

  if (!data) {
    return <div className="stack">{error ? <ErrorNote error={error} /> : <Skeleton height={160} />}</div>;
  }

  const installedMap = new Map((data.installed ?? []).map((p) => [p.name, p]));
  const catalogList = data.catalog ?? [];
  const catalogNames = new Set(catalogList.map((c) => c.name));
  // 合并目录项与本地已装但不在目录里的插件
  const items = [
    ...catalogList.map((c) => ({ name: c.name, cat: c, inst: installedMap.get(c.name) ?? null })),
    ...(data.installed ?? [])
      .filter((p) => !catalogNames.has(p.name))
      .map((p) => ({ name: p.name, cat: null, inst: p })),
  ];

  const install = (name) =>
    run(() =>
      api.post(`/plugins/_manage/${encodeURIComponent(name)}/install`).then(
        () => load(false),
        (e) => toast(errorText(t, e), 'error'),
      ),
    );

  const uninstall = async (p) => {
    const title = pickLocalized(p.inst?.title || p.cat?.title, i18n.language) || p.name;
    const ok = await confirm({
      title: t('plugins.uninstall_q', { name: title }),
      body: t('plugins.uninstall_body'),
      confirm: t('plugins.uninstall'),
      danger: true,
    });
    if (!ok) return;
    await run(() =>
      api.post(`/plugins/_manage/${encodeURIComponent(p.name)}/uninstall`, { remove_data: false }).then(
        () => {
          reloadStatus();
          return load(true);
        },
        (e) => toast(errorText(t, e), 'error'),
      ),
    );
  };

  const toggleRun = (p) => {
    const act = p.inst?.process_running ? 'stop' : 'start';
    run(() =>
      api.post(`/plugins/_manage/${encodeURIComponent(p.name)}/${act}`).then(
        () => {
          reloadStatus();
          return load(false);
        },
        (e) => toast(errorText(t, e), 'error'),
      ),
    );
  };

  const loggedIn = Boolean(status?.account?.logged_in);

  return (
    <div className="stack">
      <div className="inline" style={{ justifyContent: 'space-between' }}>
        <p className="muted" style={{ margin: 0 }}>
          {t('plugins.intro')}
        </p>
        <Button variant="ghost" className="btn-sm" icon={RefreshCw} busy={busy} onClick={() => run(() => load(true))}>
          {t('action.refresh')}
        </Button>
      </div>

      {!loggedIn && (
        <Note>
          {t('plugins.login_hint')}{' '}
          <button type="button" className="link-btn" onClick={() => navigate('/account')}>
            {t('plugins.go_login')}
          </button>
        </Note>
      )}

      {data.catalog_error && (
        <Note kind="warn">{t('plugins.catalog_failed', { reason: errorText(t, { code: data.catalog_error }) })}</Note>
      )}

      {items.length === 0 ? (
        <Empty icon={Puzzle} title={t('plugins.empty_title')}>
          <p className="muted">{t('plugins.empty_body')}</p>
        </Empty>
      ) : (
        <div className="stack">
          {items.map((item) => {
            const { name, cat, inst } = item;
            const job = data.jobs?.[name];
            const installing = job?.state === 'running';
            const Icon = pluginIcon(inst?.icon || cat?.icon);
            const title = pickLocalized(inst?.title || cat?.title, i18n.language) || name;
            const desc = pickLocalized(inst?.description || cat?.description, i18n.language);
            const supported = !cat || (cat.platforms ?? []).includes(data.target);
            const hasUpdate = Boolean(inst && cat && cat.version && cat.version !== inst.version);
            const pct = job?.total ? Math.min(100, Math.round((job.received / job.total) * 100)) : 0;
            const entry = inst?.entries?.[0];

            return (
              <Card
                key={name}
                title={
                  <span className="inline" style={{ gap: 8 }}>
                    <Icon size={18} aria-hidden="true" />
                    <span>{title}</span>
                    {inst && <span className="badge">{inst.version}</span>}
                    {inst?.running && <span className="badge badge-ok">{inst.badge || t('plugins.running')}</span>}
                    {inst?.incompatible && <span className="badge badge-warn">{t('plugins.incompatible')}</span>}
                    {!inst && cat?.requires_license && !cat.licensed && (
                      <span className="badge">
                        <Lock size={12} aria-hidden="true" /> {t('plugins.license_required')}
                      </span>
                    )}
                  </span>
                }
                sub={desc || undefined}
              >
                <div className="stack" style={{ gap: 10 }}>
                  {inst?.incompatible && (
                    <Note kind="warn">{t(`plugins.incompat.${inst.incompatible}`, { defaultValue: t('plugins.incompatible') })}</Note>
                  )}
                  {!supported && cat && (
                    <Note kind="warn">{t('plugins.unsupported_platform', { platforms: cat.platforms.join(', ') })}</Note>
                  )}
                  {installing && (
                    <div className="plg-progress">
                      <div className="muted small">
                        {t(`plugins.step.${job.step}`, { defaultValue: job.step })}
                        {job.total > 0 ? ` · ${fmtMB(job.received)} / ${fmtMB(job.total)}` : ''}
                      </div>
                      <div className="meter" role="progressbar" aria-valuenow={pct} aria-valuemin={0} aria-valuemax={100}>
                        <i style={{ width: `${pct}%` }} />
                      </div>
                    </div>
                  )}
                  {job?.state === 'failed' && (
                    <Note kind="error">{errorText(t, { code: job.error || 'INTERNAL' })}</Note>
                  )}
                  <div className="inline" style={{ flexWrap: 'wrap', gap: 8 }}>
                    {inst && !inst.incompatible && entry && (
                      <Button className="btn-sm" icon={ArrowUpRight} onClick={() => navigate(pluginRoute(name, entry.path))}>
                        {t('plugins.open')}
                      </Button>
                    )}
                    {inst && !inst.incompatible && (
                      <Button variant="ghost" className="btn-sm" icon={inst.process_running ? Square : Play} busy={busy} onClick={() => toggleRun(item)}>
                        {inst.process_running ? t('plugins.stop') : t('plugins.start')}
                      </Button>
                    )}
                    {!inst && cat && supported && (
                      loggedIn ? (
                        cat.licensed ? (
                          <Button className="btn-sm" icon={Download} busy={installing} onClick={() => install(name)}>
                            {cat.size ? t('plugins.install_size', { size: fmtMB(cat.size) }) : t('plugins.install')}
                          </Button>
                        ) : (
                          <span className="muted small">{t('plugins.not_licensed_hint')}</span>
                        )
                      ) : (
                        <Button variant="ghost" className="btn-sm" onClick={() => navigate('/account')}>
                          {t('plugins.go_login')}
                        </Button>
                      )
                    )}
                    {hasUpdate && supported && loggedIn && cat.licensed && (
                      <Button className="btn-sm" icon={Download} busy={installing} onClick={() => install(name)}>
                        {t('plugins.update_to', { version: cat.version })}
                      </Button>
                    )}
                    {inst && !hasUpdate && cat && (
                      <span className="muted small inline" style={{ gap: 4 }}>
                        <Check size={14} aria-hidden="true" />
                        {t('plugins.up_to_date')}
                      </span>
                    )}
                    {inst && (
                      <Button variant="ghost" className="btn-sm" icon={Trash2} busy={busy} onClick={() => uninstall(item)}>
                        {t('plugins.uninstall')}
                      </Button>
                    )}
                  </div>
                </div>
              </Card>
            );
          })}
        </div>
      )}
    </div>
  );
}
