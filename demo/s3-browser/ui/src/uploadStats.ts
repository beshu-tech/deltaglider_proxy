/**
 * Pure "Upload Session Statistics" aggregation for the upload page. React-
 * and SDK-free so scripts/upload-stats-regression-test.mjs can check it.
 */
import { summarizeObjectSavings } from './savings';

export interface UploadStatsInput {
  status: string;
  originalSize: number;
  /**
   * Bytes the proxy stored, from a HEAD after the upload (`dg-delta-size`
   * for a delta, else the original size). `undefined` = HEAD pending or failed.
   */
  storedSize?: number;
  /**
   * Set when this upload became its folder's baseline (see
   * `isBaselineObject`): the proxy stored the whole file once more as the
   * folder's reference. One value per folder baseline, so two uploads that
   * report the same baseline count it once.
   */
  baselineKey?: string;
}

export interface UploadStats {
  uploaded: number;
  originalSize: number;
  /** Sum of stored sizes, or null while any completed upload's size is unknown. */
  storedSize: number | null;
  /** Savings % (object view, see savings.ts), or null while storedSize is. */
  savingsPct: number | null;
  /** Folder baselines this session created; their full bytes are in storedSize. */
  baselineCount: number;
}

export function uploadSessionStats(items: UploadStatsInput[]): UploadStats {
  const completed = items.filter((item) => item.status === 'success');
  const originalSize = completed.reduce((sum, item) => sum + item.originalSize, 0);
  const known = completed.every((item) => item.storedSize !== undefined);
  // A new baseline stores the full file once more, as the folder's reference.
  // Without it, the first upload into a new folder reported ~99.9% saved.
  const baselines = new Map<string, number>();
  for (const item of completed) {
    if (item.baselineKey && !baselines.has(item.baselineKey)) baselines.set(item.baselineKey, item.originalSize);
  }
  const baselineBytes = [...baselines.values()].reduce((sum, n) => sum + n, 0);
  const storedSize = known
    ? completed.reduce((sum, item) => sum + (item.storedSize ?? 0), 0) + baselineBytes
    : null;
  return {
    uploaded: completed.length,
    originalSize,
    storedSize,
    savingsPct: storedSize === null ? null : summarizeObjectSavings(originalSize, storedSize).pct,
    baselineCount: baselines.size,
  };
}
