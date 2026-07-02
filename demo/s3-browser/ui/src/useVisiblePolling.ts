import { useEffect, useRef } from 'react';

/**
 * Run `callback` on a `ms` interval, but ONLY while the tab is visible and
 * `enabled` is true. A backgrounded tab stops polling entirely (no wasted
 * requests) and fires one immediate refresh when it becomes visible again, so
 * the operator sees fresh data on return without waiting a full interval.
 *
 * Replaces the raw `setInterval(fn, 3000)` in panels that polled flat-out even
 * in a hidden tab (AuditLogPanel, EventOutboxPanel). react-query pollers get
 * the same behaviour for free via `refetchIntervalInBackground: false`.
 */
export function useVisiblePolling(callback: () => void, ms: number, enabled = true) {
  const cbRef = useRef(callback);
  cbRef.current = callback;

  useEffect(() => {
    if (!enabled) return;

    let timer: ReturnType<typeof setInterval> | null = null;
    const start = () => {
      if (timer !== null) return;
      timer = setInterval(() => cbRef.current(), ms);
    };
    const stop = () => {
      if (timer === null) return;
      clearInterval(timer);
      timer = null;
    };

    const onVisibility = () => {
      if (document.hidden) {
        stop();
      } else {
        cbRef.current(); // catch up immediately on return
        start();
      }
    };

    // Only poll if we start out visible; otherwise wait for visibilitychange.
    if (!document.hidden) start();
    document.addEventListener('visibilitychange', onVisibility);
    return () => {
      stop();
      document.removeEventListener('visibilitychange', onVisibility);
    };
  }, [ms, enabled]);
}
