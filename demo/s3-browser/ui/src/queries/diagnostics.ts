/**
 * Diagnostics queries: /metrics, /stats, audit ring.
 *
 * MetricsPage and AnalyticsSection used to hand-roll polling via
 * `useEffect(setInterval(fetch, 5000))` — error-prone, doesn't dedupe
 * across panels, and the cleanup-on-unmount got skipped on rapid
 * route changes (B4 from the React side-quest). With Query, the
 * polling cadence lives on a single `refetchInterval` config.
 */
import { useQuery } from '@tanstack/react-query';
import { qk } from './keys';

/** Prometheus text endpoint — MetricsPage transforms the body itself. */
export function useMetricsText(refetchIntervalMs: number | false) {
  return useQuery<string>({
    queryKey: qk.metrics(),
    queryFn: async () => {
      const res = await fetch('/_/metrics', { credentials: 'include' });
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      return res.text();
    },
    refetchInterval: refetchIntervalMs,
    // Don't refetch on focus when polling is active — the interval
    // covers it and we'd double-fetch.
    refetchOnWindowFocus: refetchIntervalMs === false,
  });
}

export interface StatsResponse {
  // Keep the response shape opaque — the consumer downcasts.
  [key: string]: unknown;
}

/** Aggregate /_/stats payload. Polled at 60s by default. */
export function useStats() {
  return useQuery<StatsResponse>({
    queryKey: qk.stats(),
    queryFn: async () => {
      const res = await fetch('/_/stats', { credentials: 'include' });
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      return res.json();
    },
    refetchInterval: 60_000,
    staleTime: 30_000,
  });
}
