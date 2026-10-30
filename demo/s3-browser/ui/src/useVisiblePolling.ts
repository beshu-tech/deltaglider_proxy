import { useEffect, useState } from 'react';

/**
 * True while the tab is visible; follows `visibilitychange`. Pollers are
 * react-query `refetchInterval`s, which pause in a background tab
 * (`refetchIntervalInBackground: false`); this is for the streams (SSE).
 */
export function useDocumentVisible(): boolean {
  const [visible, setVisible] = useState(() => !document.hidden);
  useEffect(() => {
    const onVisibility = () => setVisible(!document.hidden);
    document.addEventListener('visibilitychange', onVisibility);
    return () => document.removeEventListener('visibilitychange', onVisibility);
  }, []);
  return visible;
}
