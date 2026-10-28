import { useCallback, useEffect, useRef, useState } from 'react';
import { readStorage, removeStorage, writeStorage } from './safeStorage';
import {
  SIDEBAR_DEFAULT_WIDTH,
  SIDEBAR_WIDTH_STORAGE_KEY,
  clampSidebarWidth,
  parseStoredWidth,
  sidebarMaxWidth,
} from './sidebarLayout';

export interface SidebarWidth {
  /** The width to render: the operator's choice, held inside this viewport. */
  width: number;
  /** The widest width this viewport allows. */
  maxWidth: number;
  /** Shows a new width. It is not stored until `save`. */
  resize: (px: number) => void;
  /** Stores the last `resize` width (a drag stores once, at its end). */
  save: () => void;
  /** Back to the default width; forgets the stored one. */
  reset: () => void;
}

/** The browser sidebar width, stored in localStorage. The stored value is the
 *  operator's choice; a narrow viewport only limits how it renders, so the
 *  sidebar is wide again when the window is. */
export function useSidebarWidth(): SidebarWidth {
  const [viewport, setViewport] = useState(() => window.innerWidth);
  const [preferred, setPreferred] = useState(
    () => parseStoredWidth(readStorage(SIDEBAR_WIDTH_STORAGE_KEY)) ?? SIDEBAR_DEFAULT_WIDTH,
  );
  // `save` can run in the same event as `resize`, before the re-render.
  const preferredRef = useRef(preferred);

  useEffect(() => {
    const onResize = () => setViewport(window.innerWidth);
    window.addEventListener('resize', onResize);
    return () => window.removeEventListener('resize', onResize);
  }, []);

  const resize = useCallback((px: number) => {
    preferredRef.current = clampSidebarWidth(px, window.innerWidth);
    setPreferred(preferredRef.current);
  }, []);
  const save = useCallback(() => writeStorage(SIDEBAR_WIDTH_STORAGE_KEY, String(preferredRef.current)), []);
  const reset = useCallback(() => {
    preferredRef.current = SIDEBAR_DEFAULT_WIDTH;
    setPreferred(SIDEBAR_DEFAULT_WIDTH);
    removeStorage(SIDEBAR_WIDTH_STORAGE_KEY);
  }, []);

  return {
    width: clampSidebarWidth(preferred, viewport),
    maxWidth: sidebarMaxWidth(viewport),
    resize,
    save,
    reset,
  };
}
