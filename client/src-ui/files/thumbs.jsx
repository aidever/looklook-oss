// 文件列表的缩略图（设计稿 §7.1）：图标视图里图片、视频显示缩略图（/fs/thumb，电脑上缩好、缓存）。
// 滚到才取（IntersectionObserver），同时最多取 4 张；取回来变成 blob 地址（远程、多台电脑时普通链接带不了 X-LL-Device）。
// 做不出来（旧版看看、没装 ffmpeg 的视频、坏图）就显示原来的图标。
import { useEffect, useRef, useState } from 'react';

import { fetchRaw } from '../shared/api.js';

const MAX = 4;
let running = 0;
const queue = [];
/** 按路径 + 修改时间记住取回来的（切文件夹再回来不重取）；最多 300 张 */
const cache = new Map();

function pump() {
  while (running < MAX && queue.length) {
    const job = queue.shift();
    if (job.cancelled) continue;
    running += 1;
    job.run().finally(() => {
      running -= 1;
      pump();
    });
  }
}

function remember(key, url) {
  cache.set(key, url);
  if (cache.size > 300) {
    const [old, u] = cache.entries().next().value;
    cache.delete(old);
    if (u) URL.revokeObjectURL(u);
  }
}

export function Thumb({ path, mtime, device, fallback }) {
  const key = `${device ?? ''}\n${path}\n${mtime ?? ''}`;
  const [url, setUrl] = useState(() => cache.get(key));
  const box = useRef(null);
  useEffect(() => {
    if (cache.has(key)) {
      setUrl(cache.get(key));
      return undefined;
    }
    const el = box.current;
    if (!el) return undefined;
    let job = null;
    let alive = true;
    const io = new IntersectionObserver(
      (ents) => {
        if (!ents.some((e) => e.isIntersecting) || job) return;
        io.disconnect();
        job = {
          cancelled: false,
          run: async () => {
            try {
              const dpr = window.devicePixelRatio > 1.5 ? 256 : 128;
              const res = await fetchRaw(`/fs/thumb?path=${encodeURIComponent(path)}&size=${dpr}`, device ? { device } : {});
              const u = URL.createObjectURL(await res.blob());
              remember(key, u);
              if (alive) setUrl(u);
            } catch {
              remember(key, null);
              if (alive) setUrl(null);
            }
          },
        };
        queue.push(job);
        pump();
      },
      { rootMargin: '200px' },
    );
    io.observe(el);
    return () => {
      alive = false;
      io.disconnect();
      if (job) job.cancelled = true;
    };
  }, [key]);
  return (
    <span className="fb-thumb" ref={box} aria-hidden="true">
      {url ? <img src={url} alt="" draggable={false} /> : fallback}
    </span>
  );
}
