import { useLayoutEffect, useState } from 'react';
import type { RefObject } from 'react';

interface OverlayPosition {
  top: number;
  left: number;
  width: number;
}

export function useFixedOverlayPosition(
  anchorRef: RefObject<HTMLElement | null>,
  open: boolean,
  offset = 2
): OverlayPosition {
  const [position, setPosition] = useState<OverlayPosition>({ top: 0, left: 0, width: 0 });

  useLayoutEffect(() => {
    if (!open || !anchorRef.current) return;

    const update = () => {
      const rect = anchorRef.current?.getBoundingClientRect();
      if (!rect) return;
      setPosition({ top: rect.bottom + offset, left: rect.left, width: rect.width });
    };

    update();
    window.addEventListener('resize', update);
    window.addEventListener('scroll', update, true);
    return () => {
      window.removeEventListener('resize', update);
      window.removeEventListener('scroll', update, true);
    };
  }, [anchorRef, offset, open]);

  return position;
}
