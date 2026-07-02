import { normalizeResourcePattern } from './storagePath';

/**
 * Pure row-management for the IAM permission `resources` editor
 * (ResourcePatternInput), mirroring `conditionPrefixRows.ts`.
 *
 * The server model for a permission's `resources` is a `string[]`. The editor
 * historically flattened it to a comma-joined STRING and split it back on save
 * — silently corrupting any pattern containing a literal `,` (valid in an S3
 * key) and desyncing row ids via count-based reconciliation. These helpers keep
 * the array as the ONLY wire shape: rows live in component state keyed by a
 * stable id; the array is parsed only when seeding from an external value.
 */

export interface ResourceRow {
  id: string;
  text: string;
}

let rowIdCounter = 0;

/** Monotonic, collision-free row id (stable React key; never reused). */
export function freshResourceRowId(): string {
  rowIdCounter += 1;
  return `res-${rowIdCounter}`;
}

/**
 * Seed editable rows from the persisted `string[]`. Blank entries are dropped;
 * always yields ≥1 row so the UI has an input.
 */
export function parseResourceRows(values: string[]): ResourceRow[] {
  const nonEmpty = values.filter((v) => v.trim() !== '');
  const seed = nonEmpty.length > 0 ? nonEmpty : [''];
  return seed.map((text) => ({ id: freshResourceRowId(), text }));
}

/**
 * Recombine rows into the persisted `string[]`: trim, drop blanks, de-dupe,
 * order-preserving. NO comma join — a pattern with a comma stays one entry.
 */
export function serializeResourceRows(rows: ResourceRow[]): string[] {
  const out: string[] = [];
  const seen = new Set<string>();
  for (const row of rows) {
    const text = row.text.trim();
    if (!text || seen.has(text)) continue;
    seen.add(text);
    out.push(text);
  }
  return out;
}

/** Canonicalize a single resource pattern (per-row blur). */
export function normalizeResourceRowPattern(value: string): string {
  return normalizeResourcePattern(value);
}
