// 电脑：这个账户登录着的全部电脑（数据来自看看平台，见 shared/platform.js），可以切换、改名、让一台退出登录（密码 + 邮箱验证码）。
// “切换到这台”就是顶栏的电脑切换：远程打开和本机 App 都能切（App 经本机客户端连过去）；连不上平台、只有一台时只管本机。
// 每台都能在浏览器里打开（专属地址 ?d={id} 直接连那台）。
// 下面是“共享电脑”：别人的电脑（公用机等），填地址 + 访问码加进来，由本机客户端代为转发（src/shared_pcs.rs），不登录账户也能加。
import { ExternalLink, History, KeyRound, Laptop, LogIn, LogOut, MoreHorizontal, Pencil, Plus, Trash2, Users } from 'lucide-react';
import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';

import { useStatus, webUrl } from '../App.jsx';
import { api, localApi, rtcMode } from '../shared/api.js';
import { DeviceAvatar } from '../shared/brands.jsx';
import { DevicesProvider, osLabel, useDevices } from '../shared/devices.jsx';
import { platform } from '../shared/platform.js';
import PopMenu from '../shared/PopMenu.jsx';
import { useRouter } from '../shared/router.jsx';
import { errorText } from '../shared/i18n.js';
import { Button, Card, Empty, ErrorNote, Field, Modal, More, Note, PasswordInput, Skeleton, Time, useAction, useConfirm, useLoad, useToast } from '../shared/ui.jsx';
import InstallKeysCard from './InstallKeys.jsx';
import { SourceTag } from './Membership.jsx';

/** 这台电脑的专属地址，直接连到指定的电脑。 */
export function deviceUrl(base, id) {
  return `${(base || '').replace(/\/$/, '')}/?d=${encodeURIComponent(id)}`;
}

function RenameModal({ device, onClose, onDone }) {
  const { t } = useTranslation();
  const toast = useToast();
  const [name, setName] = useState(device.name);
  const { busy, error, run } = useAction();
  const submit = (e) => {
    e.preventDefault();
    run(async () => {
      await platform.patch(`/devices/${device.id}`, { name: name.trim() });
      toast(t('devices.renamed'));
      onDone();
    });
  };
  return (
    <Modal open onClose={onClose} title={t('devices.rename_title')}>
      <form onSubmit={submit}>
        <Field label={t('settings.device_name')} hint={t('acct.rename_hint')}>
          <input value={name} onChange={(e) => setName(e.target.value)} maxLength={64} autoFocus placeholder={t('devices.name_placeholder')} />
        </Field>
        <ErrorNote error={error} />
        <div className="modal-actions">
          <Button variant="ghost" onClick={onClose}>
            {t('action.cancel')}
          </Button>
          <Button type="submit" busy={busy} disabled={!name.trim() || name.trim() === device.name}>
            {t('action.save')}
          </Button>
        </div>
      </form>
    </Modal>
  );
}

function RevokeModal({ device, here, email, onClose, onDone }) {
  const { t } = useTranslation();
  const toast = useToast();
  const [password, setPassword] = useState('');
  const [code, setCode] = useState('');
  const [sent, setSent] = useState(false);
  const { busy, error, run } = useAction();
  const send = () =>
    run(async () => {
      await platform.post('/revoke-code');
      setSent(true);
    });
  const submit = (e) => {
    e.preventDefault();
    run(async () => {
      await platform.post(`/devices/${device.id}/revoke`, { password, email_code: code });
      toast(t('acct.revoked'));
      onDone();
    });
  };
  return (
    <Modal open onClose={onClose} title={t('acct.revoke_title', { name: device.name })}>
      <form onSubmit={submit}>
        <p className="muted">{t('acct.revoke_explain')}</p>
        {here && <Note kind="warn">{t('acct.revoke_here')}</Note>}
        <Field label={t('acct.password')}>
          <PasswordInput autoComplete="current-password" value={password} onChange={(e) => setPassword(e.target.value)} />
        </Field>
        <Field label={t('acct.email_code')} hint={sent ? t('acct.code_sent', { email }) : null}>
          <div className="row">
            <input inputMode="numeric" autoComplete="one-time-code" maxLength={6} value={code} onChange={(e) => setCode(e.target.value.replace(/\D/g, ''))} placeholder="000000" />
            <Button variant="secondary" onClick={send} disabled={busy}>
              {t(sent ? 'acct.resend_code' : 'acct.send_code')}
            </Button>
          </div>
        </Field>
        <ErrorNote error={error} />
        <div className="modal-actions">
          <Button variant="ghost" onClick={onClose}>
            {t('action.cancel')}
          </Button>
          <Button type="submit" variant="danger" busy={busy} disabled={!password || code.length < 6}>
            {t('acct.revoke_confirm')}
          </Button>
        </div>
      </form>
    </Modal>
  );
}

function ComputerRow({ d, here, current, canSwitch, hostUrl, onSwitch, onRename, onRevoke }) {
  const { t } = useTranslation();
  const tags = [];
  if (here) tags.push(<span key="here" className="badge badge-brand">{t('acct.this_computer')}</span>);
  else if (current) tags.push(<span key="cur" className="badge badge-brand">{t('acct.current')}</span>);
  tags.push(
    <span key="on" className={`badge ${d.online ? 'badge-ok' : ''}`}>
      {t(d.online ? 'devices.online' : 'devices.offline')}
    </span>,
  );
  const menu = [
    { icon: Pencil, label: t('acct.rename'), onClick: () => onRename(d) },
    ...(!here ? [{ icon: ExternalLink, label: t('acct.open_in_browser'), onClick: () => window.open(deviceUrl(hostUrl, d.id), '_blank', 'noopener') }] : []),
    { icon: LogOut, label: t('acct.revoke'), onClick: () => onRevoke(d), danger: true },
  ];
  let action = null;
  if (here || current) action = null;
  else if (canSwitch)
    action = (
      <Button variant="secondary" disabled={!d.online} onClick={() => onSwitch(d)}>
        {t('acct.switch')}
      </Button>
    );
  else
    action = (
      <a className={`btn btn-secondary ${d.online ? '' : 'disabled'}`} href={deviceUrl(hostUrl, d.id)} target="_blank" rel="noopener noreferrer" aria-disabled={!d.online || undefined}>
        <ExternalLink size={16} aria-hidden="true" />
        {t('acct.open_in_browser')}
      </a>
    );
  return (
    <li className={`computer ${here || current ? 'current' : ''}`}>
      <DeviceAvatar device={d} size={36} dot />
      <span className="grow computer-text">
        <span className="computer-title">
          <b className="ellipsis">{d.name || t('devices.unnamed')}</b>
          {tags}
        </span>
        <span className="muted small">
          {[osLabel(d.os), d.arch, d.client_version && `v${d.client_version}`].filter(Boolean).join(' · ')}
          {!d.online && d.last_heartbeat_at && (
            <>
              {' · '}
              {t('acct.last_seen')} <Time value={d.last_heartbeat_at} only="relative" />
            </>
          )}
        </span>
      </span>
      <span className="computer-actions">
        {action}
        <PopMenu items={menu} icon={MoreHorizontal} label={t('action.more')} className="btn btn-plain btn-icon" />
      </span>
    </li>
  );
}

/** 加一台共享电脑 / 改备注名 / 改访问码（`mode`：add、remark、code） */
function SharedModal({ mode, item, onClose, onDone }) {
  const { t } = useTranslation();
  const toast = useToast();
  const [url, setUrl] = useState('');
  const [code, setCode] = useState('');
  const [remark, setRemark] = useState(item?.remark ?? '');
  const { busy, error, run } = useAction();
  const submit = (e) => {
    e.preventDefault();
    run(async () => {
      if (mode === 'add') await localApi.post('/rtc/shared', { url: url.trim(), code, remark: remark.trim() });
      else await localApi.put(`/rtc/shared/${encodeURIComponent(item.id)}`, mode === 'code' ? { code } : { remark: remark.trim() });
      toast(t(mode === 'add' ? 'shared.added' : 'shared.saved'));
      onDone();
    });
  };
  const ok = mode === 'add' ? url.trim() : mode === 'remark' ? remark.trim() !== (item?.remark ?? '') : true;
  return (
    <Modal open onClose={onClose} title={t(`shared.${mode}_title`, { name: item?.name })}>
      <form onSubmit={submit}>
        {mode === 'add' && <p className="muted">{t('shared.add_explain')}</p>}
        {mode === 'add' && (
          <Field label={t('shared.url')} hint={t('shared.url_hint')}>
            <input value={url} onChange={(e) => setUrl(e.target.value)} inputMode="url" autoCapitalize="off" autoCorrect="off" spellCheck={false} autoFocus placeholder="http://192.168.1.20:7681" />
          </Field>
        )}
        {mode !== 'remark' && (
          <Field label={t('shared.code')} hint={t(mode === 'add' ? 'shared.code_hint' : 'shared.code_change_hint')}>
            <PasswordInput autoComplete="off" value={code} onChange={(e) => setCode(e.target.value)} autoFocus={mode === 'code'} />
          </Field>
        )}
        {mode !== 'code' && (
          <Field label={t('shared.remark')} hint={t('shared.remark_hint')}>
            <input value={remark} onChange={(e) => setRemark(e.target.value)} maxLength={40} autoFocus={mode === 'remark'} placeholder={item?.peer_name || t('shared.remark_placeholder')} />
          </Field>
        )}
        <ErrorNote error={error} />
        <div className="modal-actions">
          <Button variant="ghost" onClick={onClose}>
            {t('action.cancel')}
          </Button>
          <Button type="submit" busy={busy} disabled={!ok}>
            {t(mode === 'add' ? 'shared.add' : 'action.save')}
          </Button>
        </div>
      </form>
    </Modal>
  );
}

function SharedRow({ d, current, canSwitch, onSwitch, onEdit, onRemove }) {
  const { t } = useTranslation();
  const menu = [
    { icon: Pencil, label: t('shared.edit_remark'), onClick: () => onEdit('remark', d) },
    { icon: KeyRound, label: t('shared.edit_code'), onClick: () => onEdit('code', d) },
    { icon: ExternalLink, label: t('acct.open_in_browser'), onClick: () => window.open(d.url, '_blank', 'noopener') },
    { icon: Trash2, label: t('shared.remove'), onClick: () => onRemove(d), danger: true },
  ];
  return (
    <li className={`computer ${current ? 'current' : ''}`}>
      <DeviceAvatar device={d} size={36} dot />
      <span className="grow computer-text">
        <span className="computer-title">
          <b className="ellipsis">{d.name || t('devices.unnamed')}</b>
          {current && <span className="badge badge-brand">{t('acct.current')}</span>}
          <span className={`badge ${d.online ? 'badge-ok' : ''}`}>{t(d.online ? 'devices.online' : 'devices.offline')}</span>
          {!d.has_code && <span className="badge">{t('shared.no_code')}</span>}
        </span>
        <span className="muted small ellipsis">{[d.url, osLabel(d.os)].filter(Boolean).join(' · ')}</span>
        {d.problem && <span className="field-error">{errorText(t, { code: d.problem })}</span>}
      </span>
      <span className="computer-actions">
        {!current && canSwitch && (
          <Button variant="secondary" disabled={!d.online} onClick={() => onSwitch(d)}>
            {t('acct.switch')}
          </Button>
        )}
        {d.problem === 'SHARED_PC_CODE_WRONG' || d.problem === 'SHARED_PC_CODE_REQUIRED' ? (
          <Button variant="secondary" onClick={() => onEdit('code', d)}>
            {t('shared.edit_code')}
          </Button>
        ) : null}
        <PopMenu items={menu} icon={MoreHorizontal} label={t('action.more')} className="btn btn-plain btn-icon" />
      </span>
    </li>
  );
}

/** 共享电脑：只在本机打开的管理台里管（访问码存在这台电脑上，接口只说填没填） */
function SharedComputers({ onSwitch, onChanged }) {
  const { t } = useTranslation();
  const devices = useDevices();
  const confirm = useConfirm();
  const toast = useToast();
  const [data, error, reload] = useLoad(() => localApi.get('/rtc/shared'));
  const [editing, setEditing] = useState(null);
  useEffect(() => {
    const id = setInterval(reload, 15000);
    return () => clearInterval(id);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
  const changed = () => {
    setEditing(null);
    reload();
    onChanged();
  };
  const remove = async (d) => {
    if (!(await confirm({ title: t('shared.remove_title', { name: d.name }), body: t('shared.remove_body'), confirm: t('shared.remove'), danger: true }))) return;
    try {
      await localApi.del(`/rtc/shared/${encodeURIComponent(d.id)}`);
      if (devices.selected === d.id) devices.select?.(devices.def);
      toast(t('shared.removed'));
      changed();
    } catch (e) {
      toast(errorText(t, e));
    }
  };
  const items = (data?.items ?? []).filter((d) => !d.shared);
  return (
    <section>
      <div className="section-title">
        <h2>{t('shared.title')}</h2>
      </div>
      <ErrorNote error={error} />
      {items.length > 0 ? (
        <ul className="computers">
          {items.map((d) => (
            <SharedRow key={d.id} d={d} current={devices.multi && devices.selected === d.id} canSwitch={devices.multi} onSwitch={onSwitch} onEdit={(mode, item) => setEditing({ mode, item })} onRemove={remove} />
          ))}
        </ul>
      ) : (
        data && <p className="muted">{t('shared.empty')}</p>
      )}
      <div>
        <Button variant="secondary" onClick={() => setEditing({ mode: 'add' })}>
          <Users size={18} aria-hidden="true" />
          {t('shared.add_button')}
        </Button>
      </div>
      {editing && <SharedModal mode={editing.mode} item={editing.item} onClose={() => setEditing(null)} onDone={changed} />}
    </section>
  );
}

export default function Computers() {
  const { t } = useTranslation();
  const { status, reload: reloadStatus } = useStatus();
  const devices = useDevices();
  const { navigate } = useRouter();
  const guest = !status.account.logged_in;
  const [data, error, reload] = useLoad(() => (guest ? Promise.resolve(null) : platform.get('/devices')), [guest]);
  const [renaming, setRenaming] = useState(null);
  const [revoking, setRevoking] = useState(null);
  // 在线状态跟着平台走，每 15 秒刷新一次
  useEffect(() => {
    if (guest) return undefined;
    const id = setInterval(reload, 15000);
    return () => clearInterval(id);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [guest]);
  // 本机：App 里是这台电脑自己（多台时是列表的 default，选中别的电脑后状态来自那台）
  const session = status.account.session;
  const hereId = rtcMode ? null : devices.multi ? devices.def : session?.device_id;
  const currentId = devices.multi ? devices.selected : rtcMode ? session.device_id : null;
  const hostUrl = session?.user_host_url;
  const items = (data?.items ?? []).filter((d) => !d.shared);
  const max = data?.max ?? 7;
  // 颜色按登录先后分配，和远程打开时顶栏的电脑列表一致
  const ordered = [...items].sort((a, b) => (a.logged_in_at < b.logged_in_at ? -1 : a.logged_in_at > b.logged_in_at ? 1 : 0));
  const colorCtx = devices.multi ? devices : { ...devices, items: ordered };
  const switchTo = (d) => {
    devices.select(d.id);
    navigate('/');
  };
  const done = (fn) => () => {
    fn(null);
    reload();
    // 改名后顶栏的电脑列表、本机缓存的账户信息也跟着变
    devices.reload?.();
    api.get('/account').catch(() => {}).finally(reloadStatus);
  };
  const sharedChanged = () => {
    devices.reload?.();
    reloadStatus();
  };
  // 共享电脑只在本机打开的管理台里管；远程打开（网页）时看不到
  const shared = !rtcMode && <SharedComputers onSwitch={switchTo} onChanged={sharedChanged} />;
  if (guest) {
    return (
      <>
        <div className="page-head">
          <h1>{t('acct.computers')}</h1>
        </div>
        <Card>
          <div className="between">
            <span>
              <b>{t('shared.mine_guest')}</b>
              <span className="muted small" style={{ display: 'block' }}>
                {t('shared.mine_guest_body')}
              </span>
            </span>
            <Button variant="secondary" onClick={() => navigate('/account')}>
              <LogIn size={18} aria-hidden="true" />
              {t('shared.sign_in')}
            </Button>
          </div>
        </Card>
        {shared}
      </>
    );
  }
  return (
    <>
      <div className="page-head">
        <h1>{t('acct.computers')}</h1>
        <SourceTag error={error} />
      </div>
      {!rtcMode && (
        <div className="section-title">
          <h2>{t('shared.mine')}</h2>
        </div>
      )}
      <ErrorNote error={error} />
      {!data && !error ? (
        <Skeleton height={220} />
      ) : items.length === 0 ? (
        <Card>
          <Empty icon={Laptop} title={t('acct.no_computers')} />
        </Card>
      ) : (
        <>
          <p className="muted">{t(devices.multi ? 'acct.computers_lead' : 'acct.computers_lead_single', { n: items.length, max })}</p>
          <div className="meter" aria-hidden="true">
            <i style={{ width: `${Math.min(100, (items.length / max) * 100)}%` }} />
          </div>
          <DevicesProvider value={colorCtx}>
            <ul className="computers">
              {items.map((d) => (
                <ComputerRow
                  key={d.id}
                  d={d}
                  here={d.id === hereId}
                  current={d.id === currentId}
                  canSwitch={devices.multi}
                  hostUrl={hostUrl}
                  onSwitch={switchTo}
                  onRename={setRenaming}
                  onRevoke={setRevoking}
                />
              ))}
            </ul>
          </DevicesProvider>
        </>
      )}
      <Card>
        <div className="between">
          <span>
            <b>{t('acct.add_computer')}</b>
            <span className="muted small" style={{ display: 'block' }}>
              {t('acct.add_computer_body')}
            </span>
          </span>
          <a className="btn btn-secondary" href={webUrl(status, '/download')} target="_blank" rel="noopener noreferrer">
            <Plus size={18} aria-hidden="true" />
            {t('acct.download')}
          </a>
        </div>
      </Card>
      {shared}
      {/* 安装密钥（给服务器、镜像装）和登录记录：少数人用，收起来 */}
      <More summary={t('acct.more_keys')}>
        <InstallKeysCard />
      </More>
      {data?.history?.length > 0 && (
        <More summary={t('acct.history')}>
          <ul className="list">
            {data.history.map((d) => (
              <li key={`${d.id}-${d.logged_out_at}`}>
                <span className="inline">
                  <History size={18} className="muted" aria-hidden="true" />
                  <span>
                    <b>{d.name}</b>
                    <span className="muted small" style={{ display: 'block' }}>
                      {t(`acct.ended.${d.status}`, { defaultValue: d.status })} · <Time value={d.logged_out_at} />
                    </span>
                  </span>
                </span>
              </li>
            ))}
          </ul>
        </More>
      )}
      {renaming && <RenameModal device={renaming} onClose={() => setRenaming(null)} onDone={done(setRenaming)} />}
      {revoking && (
        <RevokeModal
          device={revoking}
          here={revoking.id === hereId}
          email={session?.user?.email}
          onClose={() => setRevoking(null)}
          onDone={done(setRevoking)}
        />
      )}
    </>
  );
}
