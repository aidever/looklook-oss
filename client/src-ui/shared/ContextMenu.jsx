// 电脑上的右键菜单：在行、消息上点右键弹出一列操作，格式和 PopMenu 的 items 一样（icon / label / onClick / danger / disabled / 'sep'）。
// 只在有鼠标的设备上生效；手机上不拦截（长按另有处理）。菜单画在 body 下面，按鼠标位置固定定位，超出窗口就往回挪。
// 用法：const ctx = useContextMenu(() => items); <div {...ctx.bind}> … </div> {ctx.menu}
import { useEffect, useLayoutEffect, useRef, useState } from 'react';
import { createPortal } from 'react-dom';

const hasMouse = () => typeof window !== 'undefined' && Boolean(window.matchMedia?.('(hover: hover) and (pointer: fine)').matches);

export function useContextMenu(getItems) {
  const [at, setAt] = useState(null);
  const get = useRef(getItems);
  get.current = getItems;
  const bind = {
    onContextMenu: (e) => {
      if (!hasMouse() || e.nativeEvent?.pointerType === 'touch') return;
      // 选中了文字、或者点在输入框 / 链接上：用浏览器自己的菜单（复制、粘贴、打开链接）
      if (window.getSelection?.()?.toString() || e.target.closest?.('input, textarea, a[href], [contenteditable="true"]')) return;
      const got = get.current(e);
      const items = Array.isArray(got) ? got.filter(Boolean) : [];
      if (!items.length) return;
      e.preventDefault();
      e.stopPropagation();
      setAt({ x: e.clientX, y: e.clientY, items });
    },
  };
  const menu = at && <ContextMenu {...at} onClose={() => setAt(null)} />;
  return { bind, menu };
}

function ContextMenu({ x, y, items, onClose }) {
  const ref = useRef(null);
  const [pos, setPos] = useState({ left: x, top: y, visibility: 'hidden' });
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const w = el.offsetWidth;
    const h = el.offsetHeight;
    setPos({ left: Math.max(8, Math.min(x, window.innerWidth - w - 8)), top: Math.max(8, Math.min(y, window.innerHeight - h - 8)) });
  }, [x, y]);
  useEffect(() => {
    const away = (e) => !ref.current?.contains(e.target) && onClose();
    const key = (e) => e.key === 'Escape' && onClose();
    document.addEventListener('pointerdown', away, true);
    document.addEventListener('contextmenu', away, true);
    document.addEventListener('keydown', key);
    window.addEventListener('blur', onClose);
    window.addEventListener('resize', onClose);
    window.addEventListener('scroll', onClose, true);
    return () => {
      document.removeEventListener('pointerdown', away, true);
      document.removeEventListener('contextmenu', away, true);
      document.removeEventListener('keydown', key);
      window.removeEventListener('blur', onClose);
      window.removeEventListener('resize', onClose);
      window.removeEventListener('scroll', onClose, true);
    };
  }, [onClose]);
  return createPortal(
    <ul ref={ref} className="menu-pop-list menu-pop-fixed" role="menu" style={pos} onContextMenu={(e) => e.preventDefault()}>
      {items.map((it, i) =>
        it === 'sep' ? (
          <li key={`s${i}`} className="menu-pop-sep" role="separator" />
        ) : it.label ? (
          <li key={`${i}:${it.label}`}>
            <button
              type="button"
              role="menuitem"
              className={it.danger ? 'danger' : ''}
              disabled={it.disabled}
              onClick={() => {
                onClose();
                it.onClick?.();
              }}
            >
              {it.icon && <it.icon size={17} aria-hidden="true" />}
              <span className="grow">{it.label}</span>
            </button>
          </li>
        ) : null,
      )}
    </ul>,
    document.body,
  );
}
