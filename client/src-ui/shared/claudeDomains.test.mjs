import assert from 'node:assert/strict';
import test from 'node:test';

import { CLAUDE_DOMAINS, isClaudeUrl, matchClaudeDomain } from './claudeDomains.js';

test('matches official Claude / Anthropic apex and subdomains', () => {
  const cases = [
    ['https://claude.ai/login', 'claude.ai'],
    ['https://claude.ai/api/mcp/auth_callback?code=123', 'claude.ai'],
    ['https://claude.com/cai/oauth/authorize?code=true', 'claude.com'],
    ['https://platform.claude.com/oauth/token', 'claude.com'],
    ['https://sso.claude.com/login', 'claude.com'],
    ['https://code.claude.com/docs/en/security', 'claude.com'],
    ['https://anthropic.com', 'anthropic.com'],
    ['https://console.anthropic.com/settings/keys', 'anthropic.com'],
    ['https://api.anthropic.com/v1/messages', 'anthropic.com'],
    ['https://mcp-proxy.anthropic.com/sse', 'anthropic.com'],
    ['https://artifacts.claudeusercontent.com/preview', 'claudeusercontent.com'],
    ['https://claude.gg/abc', 'claude.gg'],
    ['https://clau.de/x', 'clau.de'],
    ['https://anthro.pic/y', 'anthro.pic'],
    ['https://anthropic.statuspage.io/', 'anthropic.statuspage.io'],
    ['HTTPS://CLAUDE.AI:443/chat', 'claude.ai'],
    ['https://claude.ai./login', 'claude.ai'],
    ['http://docs.claude.com/en/home', 'claude.com'],
  ];
  for (const [url, expected] of cases) {
    assert.equal(matchClaudeDomain(url), expected, url);
    assert.equal(isClaudeUrl(url), true, url);
  }
});

test('does not false-positive on lookalike domains, query params, or non-http schemes', () => {
  const safe = [
    'https://notclaude.ai/login',
    'https://fake-anthropic.com/oauth',
    'https://myclaude.com/',
    'https://example.com/?next=https://claude.ai/login',
    'https://github.com/anthropics/claude-code',
    'https://openai.com/',
    'mailto:support@anthropic.com',
    'javascript:alert("claude.ai")',
    'not-a-url',
    '',
    null,
    undefined,
  ];
  for (const url of safe) {
    assert.equal(matchClaudeDomain(url), null, String(url));
    assert.equal(isClaudeUrl(url), false, String(url));
  }
});

test('CLAUDE_DOMAINS contains core domains', () => {
  assert.ok(CLAUDE_DOMAINS.includes('claude.ai'));
  assert.ok(CLAUDE_DOMAINS.includes('claude.com'));
  assert.ok(CLAUDE_DOMAINS.includes('anthropic.com'));
});
