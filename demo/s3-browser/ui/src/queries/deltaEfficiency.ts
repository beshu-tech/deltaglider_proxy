/**
 * Compression-health scan of one bucket (Diagnostics → Delta efficiency).
 *
 * The GET answers 200 with the report, or 202 while a background scan runs:
 * the query then polls every POLL_MS until the report lands. A 202 with
 * `scanning: false` is the failure of the last scan; the server starts no
 * new scan for a GET, so it is an error, not a reason to poll. The forced
 * re-scan (POST) resets the query so it polls the new scan from scratch.
 */
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { fetchDeltaEfficiency, triggerDeltaEfficiencyScan } from '../adminApi';
import { qk } from './keys';

/** The optimized scan of a 141-prefix bucket lands in ~3-5 s: one tick. */
const POLL_MS = 3000;

/** Ceiling for the 202 → 200 polling; sized for thousands of deltaspaces. */
const SCAN_TIMEOUT_MS = 300_000;

export function useDeltaEfficiency(
  bucket: string | undefined,
  minDeltas: number,
  opts: { enabled: boolean; startedAt: () => number },
) {
  return useQuery({
    queryKey: qk.deltaEfficiency(bucket ?? '', minDeltas),
    queryFn: async () => {
      const r = await fetchDeltaEfficiency(bucket ?? '', minDeltas);
      if ('scanning' in r && !r.scanning) throw new Error(r.error);
      if ('scanning' in r && Date.now() - opts.startedAt() > SCAN_TIMEOUT_MS) {
        throw new Error(
          `Scan timed out after ${Math.round(SCAN_TIMEOUT_MS / 1000)}s — try again or check server logs`,
        );
      }
      return r;
    },
    enabled: opts.enabled && !!bucket,
    retry: false,
    refetchInterval: (q) =>
      q.state.status !== 'error' && q.state.data && 'scanning' in q.state.data ? POLL_MS : false,
  });
}

export function useTriggerDeltaEfficiencyScan() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (v: { bucket: string; minDeltas: number }) => triggerDeltaEfficiencyScan(v.bucket, v.minDeltas),
    // Drop the old report and poll the new scan.
    onSuccess: (_r, v) => qc.resetQueries({ queryKey: qk.deltaEfficiency(v.bucket, v.minDeltas) }),
  });
}
