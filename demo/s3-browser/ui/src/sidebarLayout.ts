/**
 * Browser sidebar layout rules: its width (the operator can drag it) and the
 * bucket groups by backend (the operator can collapse them).
 * React-free so src/__tests__/sidebarLayout.test.ts can check it.
 */
import { backendChipLabel, showBackendChips } from './bucketBackend';
import type { BucketInfo } from './types';

/** Default 312 px: 25% wider than the old fixed 250 px, so typical bucket
 *  names fit without a drag. Also the width of the mobile drawer. */
export const SIDEBAR_DEFAULT_WIDTH = 312;
export const SIDEBAR_MIN_WIDTH = 240;
/** Never wider than this, and never wider than half of the viewport: the
 *  object table needs the rest. */
export const SIDEBAR_MAX_WIDTH = 640;

export const SIDEBAR_WIDTH_STORAGE_KEY = 'dg-sidebar-width';
export const COLLAPSED_GROUPS_STORAGE_KEY = 'dg-sidebar-collapsed-backends';

/** The widest the sidebar can be in a viewport of `viewportWidth` px. */
export function sidebarMaxWidth(viewportWidth: number): number {
  return Math.max(SIDEBAR_MIN_WIDTH, Math.min(SIDEBAR_MAX_WIDTH, Math.floor(viewportWidth / 2)));
}

/** `px` rounded and held inside [min, max for this viewport]. */
export function clampSidebarWidth(px: number, viewportWidth: number): number {
  if (!Number.isFinite(px)) return Math.min(SIDEBAR_DEFAULT_WIDTH, sidebarMaxWidth(viewportWidth));
  return Math.min(Math.max(Math.round(px), SIDEBAR_MIN_WIDTH), sidebarMaxWidth(viewportWidth));
}

/** A stored width, or null when it is missing or not a positive integer. */
export function parseStoredWidth(raw: string | null): number | null {
  if (raw === null || !/^\d+$/.test(raw.trim())) return null;
  const px = Number(raw);
  return px > 0 ? px : null;
}

export interface BucketGroup {
  /** The backend name, or null for buckets whose backend is not known. */
  backend: string | null;
  buckets: BucketInfo[];
}

/** The storage key of a group's collapsed state. */
export function groupKey(group: BucketGroup): string {
  return group.backend ?? '';
}

/**
 * The buckets in groups by backend, or null when a group would not help:
 * with one backend (or no backend data, for example a non-admin session)
 * every bucket would be in the same group. Groups are in name order, the
 * group of unknown backends last; buckets keep their list order.
 */
export function groupBucketsByBackend(buckets: BucketInfo[]): BucketGroup[] | null {
  if (!showBackendChips(buckets.map((b) => b.backend))) return null;
  const byBackend = new Map<string | null, BucketInfo[]>();
  for (const b of buckets) {
    const backend = backendChipLabel(b.backend);
    const group = byBackend.get(backend);
    if (group) group.push(b);
    else byBackend.set(backend, [b]);
  }
  return [...byBackend]
    .map(([backend, list]) => ({ backend, buckets: list }))
    .sort((a, b) => {
      if (a.backend === null) return 1;
      if (b.backend === null) return -1;
      return a.backend.localeCompare(b.backend, undefined, { sensitivity: 'base', numeric: true });
    });
}

/** The filter input shows from this many buckets: below it the whole list
 *  fits on one screen and a filter is one more control to read. */
export const BUCKET_FILTER_MIN = 8;

/** The query's terms, lower-cased: a bucket matches when its name holds
 *  every term (so "rel dr" finds "releases-dr"). */
export function filterTerms(query: string): string[] {
  return query.toLowerCase().split(/\s+/).filter(Boolean);
}

/** The buckets whose name holds every term of `query`; all of them for a
 *  blank query. List order is kept. */
export function filterBuckets(buckets: BucketInfo[], query: string): BucketInfo[] {
  const terms = filterTerms(query);
  if (terms.length === 0) return buckets;
  return buckets.filter((b) => {
    const name = b.name.toLowerCase();
    return terms.every((t) => name.includes(t));
  });
}

/** The stored collapsed group keys; anything malformed reads as none. */
export function parseCollapsedGroups(raw: string | null): Set<string> {
  if (!raw) return new Set();
  try {
    const parsed: unknown = JSON.parse(raw);
    return Array.isArray(parsed)
      ? new Set(parsed.filter((k): k is string => typeof k === 'string'))
      : new Set();
  } catch {
    return new Set();
  }
}

export function serializeCollapsedGroups(keys: ReadonlySet<string>): string {
  return JSON.stringify([...keys].sort());
}
