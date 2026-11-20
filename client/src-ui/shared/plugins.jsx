// 插件（src/plugins.rs）在界面上的公共部分：路由、按语言挑名字、图标。看看不认识任何具体插件，入口都来自插件的清单。
import { Box, Globe, Monitor, Puzzle, Terminal } from 'lucide-react';

import { isLocalDevice, selectedDevice } from './api.js';

/** 插件页：`/plugin/<名>` */
export const PLUGIN_ROUTE = /^\/plugin\/([a-z][a-z0-9-]{1,31})$/;
export const pluginRoute = (name) => `/plugin/${name}`;
export const parsePluginRoute = (path) => PLUGIN_ROUTE.exec(path)?.[1] ?? null;

/** 清单里的多语言文字（`{ 'zh-CN': …, 'en-US': … }`）：按界面语言挑，没有就英文，再没有随便一个 */
export function pickLocalized(map, lang) {
  if (!map || typeof map !== 'object') return '';
  return map[lang] || map[String(lang).startsWith('zh') ? 'zh-CN' : 'en-US'] || map['en-US'] || Object.values(map)[0] || '';
}

const ICONS = { globe: Globe, monitor: Monitor, terminal: Terminal, box: Box, puzzle: Puzzle };
/** 清单里的图标名（lucide 的名字），认不出用拼图 */
export const pluginIcon = (name) => ICONS[name] ?? Puzzle;

/** 插件页面的地址：在别的电脑上时带 `__lldev`（本机客户端 / 启动页按它把插件的请求转给那台电脑） */
export function pluginFrameUrl(name, device = selectedDevice()) {
  return device && !isLocalDevice(device) ? `/p/${name}/?__lldev=${encodeURIComponent(device)}` : `/p/${name}/`;
}
