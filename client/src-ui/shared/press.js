import { useRef } from 'react';

/**
 * 鼠标按下就切换，不等 click：macOS App（WKWebView）里触控板“轻点”时按下和松开几乎同时到达，焦点正从终端 iframe
 * 回到管理台，click 有时被吞掉（要点两下或用力按）。按下时已切过的，随后的 click 不再切；键盘回车只有 click，照常切换。
 * 按下时不让按钮拿走焦点，好把焦点交给会话的输入框。
 */
export function usePressActivate(fn, disabled) {
  const downAt = useRef(0);
  return {
    onPointerDown: (e) => {
      if (e.pointerType !== 'mouse' || e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || disabled) return;
      downAt.current = Date.now();
      fn();
    },
    onMouseDown: (e) => e.button === 0 && e.preventDefault(),
    onClick: () => {
      if (Date.now() - downAt.current < 1500) return;
      fn();
    },
  };
}

// 吞掉按下生效之后那一下真的 click（别再点一次，也别落到菜单关掉后露出来的东西上）；按得久也算同一下
let unswallow = null;
function swallowNextClick() {
  unswallow?.();
  const kill = (e) => {
    if (!e.isTrusted) return;
    e.stopPropagation();
    e.preventDefault();
    unswallow?.();
  };
  const timer = setTimeout(() => unswallow?.(), 1500);
  document.addEventListener('click', kill, true);
  unswallow = () => {
    clearTimeout(timer);
    document.removeEventListener('click', kill, true);
    unswallow = null;
  };
}

const pressable = (e) => {
  if (e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return null;
  const b = e.target.closest?.('button');
  return b && !b.disabled && e.currentTarget.contains(b) ? b : null;
};

/**
 * 菜单（对话页的弹出层、PopMenu）整块用：里面的按钮鼠标按下就生效，道理同 usePressActivate——macOS App 里触控板轻点，
 * 焦点正在输入框 / 终端里时 click 常被吞掉，只有真按下去才行。按下时不抢焦点（输入框里的光标留着）；键盘、触屏照常走 click。
 * 只管按钮：菜单里的输入框、下拉框照常拿焦点。
 */
export const pressMenu = {
  onPointerDownCapture: (e) => {
    if (e.pointerType !== 'mouse') return;
    const b = pressable(e);
    if (!b) return;
    b.click();
    swallowNextClick();
  },
  onMouseDownCapture: (e) => pressable(e) && e.preventDefault(),
};
