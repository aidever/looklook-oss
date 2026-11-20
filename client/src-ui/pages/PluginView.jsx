// 插件页：整页一个同源 iframe（插件自带的界面，`/p/<名>/`）。没装、不兼容时说明原因，引导到插件管理。
// 多台电脑时跟着顶栏选的电脑走（换电脑时 iframe 重新加载）。
import { Puzzle } from 'lucide-react';
import { useTranslation } from 'react-i18next';

import { useStatus } from '../App.jsx';
import { useDevices } from '../shared/devices.jsx';
import { pickLocalized, pluginFrameUrl } from '../shared/plugins.jsx';
import { useRouter } from '../shared/router.jsx';
import { Button, Empty } from '../shared/ui.jsx';

export default function PluginView({ name }) {
  const { t, i18n } = useTranslation();
  const { status } = useStatus();
  const { navigate } = useRouter();
  const devices = useDevices();
  const p = (status.plugins ?? []).find((x) => x.name === name);
  if (!p || p.incompatible) {
    return (
      <Empty icon={Puzzle} title={t(p ? 'plugins.incompatible' : 'plugins.not_installed_here')}>
        <Button onClick={() => navigate('/plugins')}>{t('plugins.manage')}</Button>
      </Empty>
    );
  }
  const title = pickLocalized(p.title, i18n.language) || name;
  // key：换电脑、换语言时整个重新加载插件页面
  const src = `${pluginFrameUrl(name, devices.selected)}${pluginFrameUrl(name, devices.selected).includes('?') ? '&' : '?'}lang=${encodeURIComponent(i18n.language)}`;
  return <iframe key={src} className="plugin-frame" title={title} src={src} allow="clipboard-read; clipboard-write; fullscreen; autoplay" />;
}
