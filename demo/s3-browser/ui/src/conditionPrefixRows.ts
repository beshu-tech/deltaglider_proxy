import { normalizePrefix } from './storagePath';

/**
 * Pure row-management for the s3:prefix condition editor (ConditionPrefixInput).
 *
 * The persisted form of an s3:prefix StringLike condition is a single
 * comma-joined string (e.g. "uploads/*, ror/, ror/builds/"). Historically the
 * editor re-parsed that string into rows on EVERY keystroke and re-serialized
 * it on blur, which — combined with a stale closure over the value prop — could
 * silently drop an unrelated row when one row lost focus. These helpers keep
 * the comma string purely an OUTPUT: rows live in component state keyed by a
 * stable id, and the string is only parsed when seeding from an external value.
 */

export interface PrefixRow {
  id: string;
  text: string;
}

let rowIdCounter = 0;

/** Monotonic, collision-free row id (stable React key; never reused). */
export function freshRowId(): string {
  rowIdCounter += 1;
  return `pfx-${rowIdCounter}`;
}

/** Canonicalize a single prefix pattern, preserving a trailing `*` wildcard. */
export function normalizePrefixPattern(value: string): string {
  const trimmed = value.trim();
  if (!trimmed || trimmed === '.*' || trimmed === '*') return trimmed;
  if (trimmed.endsWith('*')) {
    const base = trimmed.slice(0, -1);
    return `${normalizePrefix(base)}*`;
  }
  return normalizePrefix(trimmed);
}

/**
 * Parse the persisted comma-joined string into editable rows (always ≥1 row).
 *
 * Splits on `,` with no escaping. This is safe because a comma is not a valid
 * character in an S3 key/prefix, so a comma in the input is always a row
 * separator, never part of a single pattern — escaping machinery would add
 * round-trip risk for a case that cannot legitimately occur.
 */
export function parseRows(value: string): PrefixRow[] {
  const parts = value.split(',').map((part) => part.trim());
  const rows = (parts.length > 0 ? parts : ['']).map((text) => ({ id: freshRowId(), text }));
  return rows.length > 0 ? rows : [{ id: freshRowId(), text: '' }];
}

/**
 * Serialize editable rows back to the persisted comma-joined string.
 * Empty rows are dropped so the persisted value never carries dangling commas;
 * a trailing empty row being edited lives in component state, not here.
 */
export function serializeRows(rows: PrefixRow[]): string {
  return rows
    .map((row) => row.text.trim())
    .filter(Boolean)
    .join(', ');
}
