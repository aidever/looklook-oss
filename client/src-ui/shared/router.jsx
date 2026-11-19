// 极简前端路由（History API）。客户端网关对非 /api 路径回落到入口 HTML。
import { createContext, useContext, useEffect, useState } from 'react';

// 返回键模型：详情页有一个“上级”（会话 → 会话列表）。
// - 进详情页时（手机）保证它下面那条历史记录就是上级：来源是别处（助理、新建会话页）就把来源那条换成上级，
//   所以系统返回键 / 返回手势永远回到上级，和左上角箭头一致；
// - 左上角箭头 up()：下面那条就是上级时直接后退（不新压一条，否则 列表→会话→箭头→列表 之后再按返回又回到会话），否则用 replace 换成上级。
const PARENTS = [
  [/^\/(t|s)\/[^/]+$/, '/'],
  [/^\/new$/, '/'],
];
export const parentOf = (path) => PARENTS.find(([re]) => re.test(path))?.[1] ?? null;
const touch = () => Boolean(window.matchMedia?.('(hover: none) and (pointer: coarse)').matches);

const Ctx = createContext({ path: '/', search: '', hash: '', navigate: () => {} });

export function Router({ children }) {
  // pop：这次变化是浏览器的前进 / 后退（不是 navigate），抽屉据此算关掉时要后退几步
  const here = (pop = false) => ({ path: window.location.pathname, search: window.location.search, hash: window.location.hash, pop });
  const [loc, setLoc] = useState(here);
  useEffect(() => {
    const on = () => setLoc(here(true));
    window.addEventListener('popstate', on);
    window.addEventListener('hashchange', on);
    return () => {
      window.removeEventListener('popstate', on);
      window.removeEventListener('hashchange', on);
    };
  }, []);
  const navigate = (to, { replace = false } = {}) => {
    const u = new URL(to, window.location.origin);
    const parent = !replace && touch() ? parentOf(u.pathname) : null;
    if (parent && window.location.pathname !== parent) window.history.replaceState(null, '', parent);
    if (replace) window.history.replaceState(null, '', to);
    else window.history.pushState(parent ? { llUp: true } : null, '', to);
    setLoc({ path: u.pathname, search: u.search, hash: u.hash, pop: false, replace });
    window.scrollTo(0, 0);
  };
  /** 回上级（左上角箭头）。没有上级的页面什么也不做。 */
  const up = () => {
    const parent = parentOf(loc.path);
    if (!parent) return;
    if (window.history.state?.llUp) window.history.back();
    else navigate(parent, { replace: true });
  };
  return <Ctx.Provider value={{ ...loc, navigate, up }}>{children}</Ctx.Provider>;
}

// 规范化地址：去掉末尾斜杠、/index.html 等入口文件名；不认识的路径一律回到首页。
export function canonicalPath(path, isKnown) {
  let p = path.replace(/\/index\.html$/, '/');
  if (p.length > 1) p = p.replace(/\/+$/, '');
  return isKnown(p) ? p : '/';
}

// 地址不规范时用 replace 改写，不留下历史记录；返回规范后的路径。
export function useCanonicalPath(isKnown) {
  const { path, search, navigate } = useContext(Ctx);
  const canon = canonicalPath(path, isKnown);
  useEffect(() => {
    if (canon !== path) navigate(canon === '/' ? '/' : canon + search, { replace: true });
  }, [canon, path]);
  return canon;
}

export function useRouter() {
  return useContext(Ctx);
}

export function Link({ to, children, className, ...rest }) {
  const { navigate, path } = useRouter();
  const active = path === to;
  return (
    <a
      href={to}
      className={`${className ?? ''}${active ? ' active' : ''}`}
      onClick={(e) => {
        if (e.metaKey || e.ctrlKey || e.shiftKey || e.button !== 0) return;
        e.preventDefault();
        navigate(to);
      }}
      {...rest}
    >
      {children}
    </a>
  );
}

export function query(search, key) {
  return new URLSearchParams(search).get(key);
}
