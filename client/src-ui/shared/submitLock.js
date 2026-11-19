// 回答审批 / 提问 / 选择框用的“点一下就锁住”：
// - 同步的锁（ref）：连点、双击、回车 + 点击同时来，只发出去一次（state 要等下一次渲染才生效，挡不住同一帧里的第二下）；
// - 送到以后继续锁着、显示“已提交”，直到这张卡片换了（key 变了：状态更新把它收走 / 换成下一个问题），
//   不然请求很快返回、卡片却要等状态推过来才消失，这段时间看着像没点上，用户会再点；
// - 一直没收走（状态更新丢了）：过一会儿解锁，让用户能再点。
import { useCallback, useEffect, useRef, useState } from 'react';

export function useSubmitLock(key, { hold = 12_000 } = {}) {
  const [state, setState] = useState(null); // { what, sent }
  const lock = useRef(null);
  const keyRef = useRef(key);
  useEffect(() => {
    keyRef.current = key;
    lock.current = null;
    setState(null);
  }, [key]);
  useEffect(() => {
    if (!state?.sent) return undefined;
    const tm = setTimeout(() => {
      lock.current = null;
      setState(null);
    }, hold);
    return () => clearTimeout(tm);
  }, [state, hold]);
  /** `fn` 抛 `CHAT_STALE`（别处已经答过了）也当送到；其余错误解锁后照样抛出去 */
  const run = useCallback(async (what, fn) => {
    if (lock.current != null) return false;
    const at = keyRef.current;
    lock.current = what;
    setState({ what, sent: false });
    const done = () => {
      if (keyRef.current === at) setState({ what, sent: true });
    };
    try {
      await fn();
      done();
      return true;
    } catch (e) {
      if (e?.code === 'CHAT_STALE') {
        done();
        return true;
      }
      if (keyRef.current === at) {
        lock.current = null;
        setState(null);
      }
      throw e;
    }
  }, []);
  return { busy: state != null, sent: Boolean(state?.sent), what: state?.what ?? null, run };
}
