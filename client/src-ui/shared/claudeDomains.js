// Claude / Anthropic 官方相关域名集合（命中后触发红色风控警告、复制提醒与本机打开二次确认）。
// 仅按 URL 的 hostname 精确匹配根域名或其任意子域名，避免把普通网址里的路径/参数误判为命中。
export const CLAUDE_DOMAINS = Object.freeze([
  'claude.ai',
  'claude.com',
  'anthropic.com',
  'claudeusercontent.com',
  'claude.gg',
  'clau.de',
  'anthro.pic',
  'anthropic.statuspage.io',
]);

/**
 * 判断 URL 是否命中 Claude / Anthropic 相关域名。
 * 命中时返回对应的根域名（如 `'claude.ai'`），未命中或非法 URL 返回 `null`。
 */
export function matchClaudeDomain(url) {
  if (typeof url !== 'string' || !url.trim()) return null;
  let parsed;
  try {
    parsed = new URL(url);
  } catch {
    return null;
  }
  if (parsed.protocol !== 'http:' && parsed.protocol !== 'https:') return null;
  const host = parsed.hostname.toLowerCase().replace(/\.+$/, '');
  if (!host) return null;
  for (const domain of CLAUDE_DOMAINS) {
    if (host === domain || host.endsWith(`.${domain}`)) return domain;
  }
  return null;
}

export function isClaudeUrl(url) {
  return matchClaudeDomain(url) !== null;
}
