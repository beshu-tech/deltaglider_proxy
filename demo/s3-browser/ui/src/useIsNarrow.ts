import { useEffect, useState } from 'react';
import { BREAKPOINT } from './components/shared-styles';

/**
 * True when the viewport is narrower than `breakpoint` px. Shared by the admin
 * shell (mobile drawer at 900px) and any panel that needs to collapse a
 * one-line layout to a wrapped one (e.g. the permission grant rows).
 */
export function useIsNarrow(breakpoint: number = BREAKPOINT.shell): boolean {
  const [narrow, setNarrow] = useState(() =>
    typeof window !== 'undefined' ? window.innerWidth < breakpoint : false,
  );
  useEffect(() => {
    const onResize = () => setNarrow(window.innerWidth < breakpoint);
    window.addEventListener('resize', onResize);
    return () => window.removeEventListener('resize', onResize);
  }, [breakpoint]);
  return narrow;
}
