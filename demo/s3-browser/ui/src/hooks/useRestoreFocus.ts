import { useEffect, useRef } from 'react';

/**
 * While `active`, remember what had focus before; when it ends (or the
 * component unmounts), put focus back there, else on the main region.
 * Without it, an overlay closed by Escape or a URL change left focus on
 * <body> and the next Tab started from the top of the page.
 */
export function useRestoreFocus(active: boolean): void {
  // Read during render, on the render that opens: by the time an effect runs,
  // the overlay's autoFocus already moved focus inside it.
  const opener = useRef<HTMLElement | null>(null);
  const wasActive = useRef(false);
  if (active && !wasActive.current) {
    const el = typeof document !== 'undefined' ? document.activeElement : null;
    opener.current = el instanceof HTMLElement && el !== document.body ? el : null;
  }
  wasActive.current = active;

  useEffect(() => {
    if (!active) return;
    return () => {
      const target = opener.current;
      // After the overlay's own teardown (AntD moves focus as it closes).
      setTimeout(() => {
        const current = document.activeElement;
        const lost = !current || current === document.body || !current.isConnected
          || current.closest('.ant-drawer, .ant-modal-root, .ant-modal-wrap') !== null;
        if (!lost) return; // something else took focus on purpose
        if (target?.isConnected) target.focus();
        else document.getElementById('main-content')?.focus();
      }, 0);
    };
  }, [active]);
}
