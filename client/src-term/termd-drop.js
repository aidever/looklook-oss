// 把文件拖到 termd 会话的终端上：自动敲 trz 上传（和 xterm 那条路的 TrzszAddon 一样）。
// 嵌在管理台里时 term-inject.js 在捕获阶段先接管拖放（走看看自己的上传），到不了这里。
export function attachDrop(el, conn) {
  el.addEventListener('dragover', (e) => {
    if (conn.trz) e.preventDefault();
  });
  el.addEventListener('drop', (e) => {
    if (!conn.trz || !e.dataTransfer?.items) return;
    e.preventDefault();
    conn.uploadFiles(e.dataTransfer.items);
  });
}
