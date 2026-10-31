/**
 * The raw Prometheus text of `/_/metrics` (the dashboard parses it). The
 * dashboard sets the poll interval from its cadence picker, and only while
 * the Monitoring view is on screen. No retry: a failure shows at once.
 */
import { useQuery } from '@tanstack/react-query';
import { fetchWithRelogin } from '../adminApi/core';
import { throwApiError } from '../errorHandling';
import { qk } from './keys';

async function fetchMetricsText(): Promise<string> {
  const res = await fetchWithRelogin('/_/metrics', { credentials: 'include' });
  if (!res.ok) await throwApiError(res, 'Metrics scrape');
  return res.text();
}

export function usePrometheusMetrics(refetchInterval: number | false) {
  return useQuery({
    queryKey: qk.metrics(),
    queryFn: fetchMetricsText,
    refetchInterval,
    retry: false,
  });
}
