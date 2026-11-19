// 安全网拦下的命令：卡片上写清楚是看看的安全网拦的、哪条规则（内置的还是自己加的），并给去安全网调整的入口。
// 对话页的确认卡片和看板共用。`from` 是后端 pending 的 guard_from（{ rule, builtin }）；旧版本只有 guard（规则文本）。
import { ShieldAlert } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { Link } from './router.jsx';

export default function GuardNote({ from, rule, className = 'chat-guard' }) {
  const { t } = useTranslation();
  const r = from?.rule ?? rule;
  const key = from?.builtin ? 'chat.guard_from_builtin' : from ? 'chat.guard_from_mine' : 'chat.guard_from';
  return (
    <p className={className}>
      <ShieldAlert size={14} aria-hidden="true" style={{ flexShrink: 0 }} />
      <span>
        {t(key, { rule: r })}{' '}
        <Link to="/ai#guard" className="chat-guard-link">
          {t('chat.guard_adjust')}
        </Link>
      </span>
    </p>
  );
}
