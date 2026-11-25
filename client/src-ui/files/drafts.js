// 编辑中的草稿（设计稿 §4.2）：改了没存就记在这个浏览器的 IndexedDB 里（停手 0.5 秒记一次），
// 刷新、断线、不小心关了再打开同一个文件时提示“恢复 / 丢掉”。按电脑 + 路径存，带上开始编辑时文件的修改时间；
// 存好了、丢掉了就删；7 天没动的自动清掉。IndexedDB 用不了（隐私模式等）时什么都不做。

const DB = 'll-drafts';
const STORE = 'drafts';
const KEEP_MS = 7 * 86_400_000;
let dbp = null;

function open() {
  if (!dbp) {
    dbp = new Promise((resolve) => {
      try {
        const r = indexedDB.open(DB, 1);
        r.onupgradeneeded = () => r.result.createObjectStore(STORE);
        r.onsuccess = () => resolve(r.result);
        r.onerror = () => resolve(null);
      } catch {
        resolve(null);
      }
    });
    dbp.then((db) => db && prune(db));
  }
  return dbp;
}

function tx(db, mode, fn) {
  return new Promise((resolve) => {
    try {
      const t = db.transaction(STORE, mode);
      const req = fn(t.objectStore(STORE));
      t.oncomplete = () => resolve(req?.result);
      t.onerror = () => resolve(undefined);
    } catch {
      resolve(undefined);
    }
  });
}

function prune(db) {
  try {
    const t = db.transaction(STORE, 'readwrite');
    const cur = t.objectStore(STORE).openCursor();
    cur.onsuccess = () => {
      const c = cur.result;
      if (!c) return;
      if (!c.value?.at || Date.now() - c.value.at > KEEP_MS) c.delete();
      c.continue();
    };
  } catch {
    /* 清不掉下次再清 */
  }
}

const key = (device, path) => `${device || 'self'}\n${path}`;

/** 读草稿：`{ text, mtime, at }` 或 null */
export async function getDraft(device, path) {
  const db = await open();
  return (db && (await tx(db, 'readonly', (s) => s.get(key(device, path))))) ?? null;
}

export async function putDraft(device, path, text, mtime) {
  const db = await open();
  if (db) await tx(db, 'readwrite', (s) => s.put({ text, mtime: mtime ?? null, at: Date.now() }, key(device, path)));
}

export async function dropDraft(device, path) {
  const db = await open();
  if (db) await tx(db, 'readwrite', (s) => s.delete(key(device, path)));
}
