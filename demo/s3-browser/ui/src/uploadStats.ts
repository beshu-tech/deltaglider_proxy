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
}

export interface UploadStats {
  uploaded: number;
  originalSize: number;
  /** Sum of stored sizes, or null while any completed upload's size is unknown. */
  storedSize: number | null;
  /** Savings % (object view, see savings.ts), or null while storedSize is. */
  savingsPct: number | null;
}

export function uploadSessionStats(items: UploadStatsInput[]): UploadStats {
  const completed = items.filter((item) => item.status === 'success');
  const originalSize = completed.reduce((sum, item) => sum + item.originalSize, 0);
  const known = completed.every((item) => item.storedSize !== undefined);
  const storedSize = known ? completed.reduce((sum, item) => sum + (item.storedSize ?? 0), 0) : null;
  return {
    uploaded: completed.length,
    originalSize,
    storedSize,
    savingsPct: storedSize === null ? null : summarizeObjectSavings(originalSize, storedSize).pct,
  };
}
