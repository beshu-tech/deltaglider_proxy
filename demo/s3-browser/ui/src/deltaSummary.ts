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

/**
 * The headline of the inspector's storage card for one object.
 *
 * The folder's baseline is stored whole: it is the reference that the other
 * versions are deltas against. A "0.0% saved" in success green and a
 * "Delta" chip told three contradicting things about it; it gets a neutral
 * "Stored whole" headline and a "Baseline" chip instead.
 */
export type ObjectStorageHeadline =
  | { kind: 'baseline'; title: string; detail: string; chip: string }
  | { kind: 'savings'; title: string; detail: string; chip: string };

export function objectStorageHeadline(o: {
  storageType: string | undefined;
  isBaseline: boolean;
  savingsPct: number;
  savedBytesText: string;
}): ObjectStorageHeadline {
  if (o.storageType === 'delta' && o.isBaseline) {
    return {
      kind: 'baseline',
      title: 'Stored whole',
      detail: "Reference for this folder's deltas",
      chip: storageChip('delta', true).label,
    };
  }
  return {
    kind: 'savings',
    title: `${o.savingsPct.toFixed(1)}%`,
    detail: `${o.savedBytesText} saved`,
    chip: storageChip(o.storageType || 'passthrough', false).label,
  };
}

/**
 * THE storage chip of one object, for the object table and the inspector:
 * "Baseline" (neutral: the folder's reference, stored whole), "Delta",
 * "Original" (stored whole, not delta-eligible) or "—" (not known). The
 * table showed "Delta" for a file the inspector called "Baseline".
 */
export function storageChip(storageType: string | undefined, isBaseline: boolean): {
  label: string;
  /** `neutral` = not a storage-type colour (baseline, unknown). */
  tone: 'neutral' | 'type';
  title?: string;
} {
  if (!storageType) return { label: '—', tone: 'neutral', title: 'Storage type not known' };
  if (storageType === 'delta' && isBaseline) {
    return { label: 'Baseline', tone: 'neutral', title: "Stored whole: the reference for this folder's deltas" };
  }
  if (storageType === 'passthrough') return { label: 'Original', tone: 'type' };
  return { label: storageType.charAt(0).toUpperCase() + storageType.slice(1), tone: 'type' };
}
