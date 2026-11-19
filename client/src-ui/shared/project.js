// 项目相关的小工具（会话的“网页”标签按进程工作目录归到项目，见 pages/Pages.jsx 的 ProjectPages）

/** 进程的工作目录在项目里（项目本身或下面的子目录）。Windows 不分大小写、斜杠两种都认 */
export function inProject(cwd, workdir) {
  if (!cwd || !workdir) return false;
  const norm = (x) => {
    let v = x.replace(/\\/g, '/').replace(/\/+$/, '');
    if (/^[a-z]:/i.test(v)) v = v.toLowerCase();
    return v;
  };
  const c = norm(cwd);
  const w = norm(workdir);
  return c === w || c.startsWith(`${w}/`);
}
