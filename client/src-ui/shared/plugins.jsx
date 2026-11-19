// 通用插件前端工具：图标映射、多语言取值、路由与 iframe 地址拼接。
import { Activity, Bot, FolderOpen, Globe, Globe2, Monitor, Puzzle, Server, Shield, Terminal, Wrench } from 'lucide-react';

const ICONS = {
  globe: Globe2,
  web: Globe,
  monitor: Monitor,
  puzzle: Puzzle,
  terminal: Terminal,
  wrench: Wrench,
  bot: Bot,
  folder: FolderOpen,
  server: Server,
  shield: Shield,
  activity: Activity,
};

export function pluginIcon(name) {
  return ICONS[String(name || '').toLowerCase()] || Puzzle;
}

export function pickLocalized(obj, lang = 'zh-CN') {
  if (!obj) return '';
  if (typeof obj === 'string') return obj;
  if (String(lang).toLowerCase().startsWith('en') && obj['en-US']) return obj['en-US'];
  return obj['zh-CN'] || obj['en-US'] || '';
}

export const PLUGIN_ROUTE = /^\/plugin\/([a-z][a-z0-9-]{1,31})(?:\/(.*))?$/;

export function parsePluginRoute(path) {
  const m = String(path || '').match(PLUGIN_ROUTE);
  if (!m) return null;
  return { name: m[1], entryPath: m[2] || '' };
}

export function pluginRoute(name, entryPath = '') {
  const rel = String(entryPath || '').replace(/^\/+/, '');
  return rel ? `/plugin/${name}/${rel}` : `/plugin/${name}`;
}

/** 构造同源插件 iframe 地址：多台电脑时始终带上 __lldev（包括本机），让本机 to_peer / 远程 boot.js 更新路由归属。 */
export function pluginIframeSrc(name, entryPath = '', deviceId = null, extraQuery = '') {
  const rel = String(entryPath || '').replace(/^\/+/, '');
  const base = `/p/${name}/${rel}`;
  const params = new URLSearchParams(extraQuery ? extraQuery.replace(/^\?/, '') : '');
  if (deviceId) params.set('__lldev', deviceId);
  const q = params.toString();
  return q ? `${base}?${q}` : base;
}

export const UI_MESSAGE_TAG = 'plugin';
