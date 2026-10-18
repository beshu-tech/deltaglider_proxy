import type { PrefixSavingsResponse } from './adminApi';

/**
 * Shape consumed by the DeltaSavingsChip. The fields are a flattened
 * view of the server's `PrefixSavingsResponse` (which itself wraps
 * `SavingsTotals` — the single source of truth in
 * `src/deltaglider/savings.rs`). All client-side aggregation is gone:
 * the math is reference-aware and lives once, server-side.
 *
 * Lives in its own file (not next to the component) so React Fast
 * Refresh stays happy with the component file exporting only React
 * components.
 */
export interface DeltaSummary {
  /** Number of delta-compressed objects under the current prefix. */
  deltaCount: number;
  /** Number of reference baselines under the current prefix. */
  referenceCount: number;
  /** Number of user-visible objects (deltas + passthroughs). */
  totalCount: number;
  /** Sum of logical/original bytes (what users see). */
  originalBytes: number;
  /**
   * Sum of actual on-disk bytes (deltas + references + passthroughs).
   * INCLUDES reference bytes — this is what "really sits on disk".
   */
  storedBytes: number;
  /** Reference bytes only (subset of storedBytes), for tooltip detail. */
  referenceBytes: number;
  /** Savings ratio 0..=99.99 from the server; null when nothing to measure. */
  savingsPct: number | null;
  /** Server walked too many objects and bailed — totals are a lower bound. */
  truncated: boolean;
  /** Some original sizes are not known: the original bytes and the savings are approximate. */
  estimated: boolean;
  /** Network/fetch in progress so the chip can show a muted state. */
  loading: boolean;
}

/** Adapter from the wire response to the chip's local shape. */
export function summaryFromResponse(resp: PrefixSavingsResponse): DeltaSummary {
  const t = resp.totals;
  return {
    deltaCount: t.delta_count,
    referenceCount: t.reference_count,
    totalCount: t.delta_count + t.passthrough_count,
    originalBytes: t.original_bytes,
    storedBytes: t.stored_bytes,
    referenceBytes: t.reference_bytes,
    savingsPct: resp.savings_percentage,
    truncated: resp.truncated,
    estimated: Boolean(resp.estimated),
    loading: false,
  };
}
