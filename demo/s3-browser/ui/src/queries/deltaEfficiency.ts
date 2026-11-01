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

/**
 * A running scan fails when it lists no new object for this long. A fixed
 * ceiling (300 s) gave up on a large bucket while the server was still
 * making progress (prod: 93 000 objects on a slow backend).
 */
const STALL_TIMEOUT_MS = 120_000;

/** The last progress the poll saw: keys listed, and when the count grew. */
export interface ScanProgress {
  keys: number;
  at: number;
}

export function useDeltaEfficiency(
  bucket: string | undefined,
  minDeltas: number,
  opts: { enabled: boolean; progress: () => ScanProgress },
) {
  return useQuery({
    queryKey: qk.deltaEfficiency(bucket ?? '', minDeltas),
    queryFn: async () => {
      const r = await fetchDeltaEfficiency(bucket ?? '', minDeltas);
      if ('scanning' in r && !r.scanning) throw new Error(r.error);
      if ('scanning' in r) {
        const seen = opts.progress();
        if (r.listed_keys > seen.keys) {
          seen.keys = r.listed_keys;
          seen.at = Date.now();
        } else if (Date.now() - seen.at > STALL_TIMEOUT_MS) {
          throw new Error(
            `The scan listed no new object for ${Math.round(STALL_TIMEOUT_MS / 1000)} s. Try again, or check the server logs.`,
          );
        }
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
