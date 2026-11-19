// 各台电脑上保存的 SSH 主机（会话栏的“SSH”标签、快速切换用）。SSH 主机是存在每台电脑上的，多台电脑时每台在线的各问一遍。
// SSH 主机页添加、修改、删除后发 `ll-ssh-hosts` 事件，这里跟着重新取。
import { useEffect, useRef, useState } from 'react';

import { deviceApi, rememberTerminals } from './api.js';
import { useDevices } from './devices.jsx';

export const SSH_HOSTS_EVENT = 'll-ssh-hosts';

/** 主机改过了（SSH 主机页调）：会话栏、快速切换重新取。 */
export const sshHostsChanged = () => window.dispatchEvent(new Event(SSH_HOSTS_EVENT));

/**
 * `enabled` 为 false 时不取（标签没打开、快速切换没打开）。返回 { hosts, loaded, reload }：
 * hosts 是 [{ ...主机, device }]（单台电脑时 device 为 null），按 SSH 主机页的顺序；取不到的电脑（旧版客户端、断开）跳过。
 */
export function useSshHosts(enabled) {
  const devices = useDevices();
  const ids = devices.multi ? devices.items.filter((d) => d.online).map((d) => d.id) : [null];
  const key = ids.join(',');
  const [state, setState] = useState({ key: null, hosts: [] });
  const [tick, setTick] = useState(0);
  const last = useRef(0);
  useEffect(() => {
    if (!enabled) return undefined;
    const again = () => setTick((n) => n + 1);
    // 切回窗口时最多半分钟取一次（主机很少变）
    const onFocus = () => Date.now() - last.current > 30e3 && again();
    window.addEventListener(SSH_HOSTS_EVENT, again);
    window.addEventListener('focus', onFocus);
    return () => {
      window.removeEventListener(SSH_HOSTS_EVENT, again);
      window.removeEventListener('focus', onFocus);
    };
  }, [enabled]);
  useEffect(() => {
    if (!enabled) return undefined;
    let alive = true;
    last.current = Date.now();
    Promise.all(
      ids.map((id) =>
        deviceApi(id)
          .get('/ssh/hosts')
          .then((r) => (r?.items ?? []).map((h) => ({ ...h, device: id })))
          .catch(() => []),
      ),
    ).then((all) => alive && setState({ key, hosts: all.flat() }));
    return () => {
      alive = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [enabled, key, tick]);
  return { hosts: state.key === key ? state.hosts : state.hosts.filter((h) => ids.includes(h.device)), loaded: state.key === key, reload: () => setTick((n) => n + 1) };
}

/** 打开连这台主机的终端：`fresh` 再开一个，否则已经开着的话回到它。返回会话 id。 */
export async function openSshHost(device, id, fresh = false) {
  const v = await deviceApi(device).post(`/ssh/hosts/${id}/open${fresh ? '?new=true' : ''}`);
  const sid = v.row?.id ?? v.id;
  if (sid) rememberTerminals(device, [v.row ?? sid]);
  return sid;
}

/** 主机地址：user@host[:port] */
export const sshAddr = (h) => `${h.user ? `${h.user}@` : ''}${h.host}${h.port && h.port !== 22 ? `:${h.port}` : ''}`;
