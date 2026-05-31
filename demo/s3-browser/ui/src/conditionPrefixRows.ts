import { normalizePrefixPreserveTrailingSlash } from './storagePath';

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

/**
 * Canonicalize a single s3:prefix pattern, preserving a trailing `*`
 * wildcard AND the operator's trailing-slash choice.
 *
 * For an `s3:prefix StringLike` condition, `ror/libs` and `ror/libs/`
 * are NOT equivalent: the slash-less form also matches sibling keys
 * like `ror/libs-internal/…`, while the trailing-slash form scopes to
 * the folder. We therefore only clean separators (collapse `//`,
 * convert `\`, trim) and must NOT auto-append `/` on blur — doing so
 * silently narrows the match the operator typed.
 */
export function normalizePrefixPattern(value: string): string {
  const trimmed = value.trim();
  if (!trimmed || trimmed === '.*' || trimmed === '*') return trimmed;
  if (trimmed.endsWith('*')) {
    const base = trimmed.slice(0, -1);
    return `${normalizePrefixPreserveTrailingSlash(base)}*`;
  }
  return normalizePrefixPreserveTrailingSlash(trimmed);
}

/**
 * Array-shaped seed/serialize for the s3:prefix editor.
 *
 * The persisted condition value is a `string[]` where the empty string `""`
 * is a MEANINGFUL entry ("list from the bucket root"). The text-row editor
 * can't represent `""` distinctly from a blank being-edited row, so the
 * component models root as a separate boolean toggle. These helpers split an
 * incoming array into `{ includeRoot, rows }` and recombine on the way out.
 */
interface PrefixArrayState {
  /** True when the persisted value contains the `""` (root) entry. */
  includeRoot: boolean;
  /** Non-root prefixes as editable rows (always ≥1 so the UI has an input). */
  rows: PrefixRow[];
}

export function parseRowsArray(values: string[]): PrefixArrayState {
  const includeRoot = values.some((v) => v === '');
  const nonRoot = values.filter((v) => v !== '' && v.trim() !== '');
  const rows = (nonRoot.length > 0 ? nonRoot : ['']).map((text) => ({ id: freshRowId(), text }));
  return { includeRoot, rows };
}

/**
 * Recombine text rows + the root toggle into the persisted `string[]`.
 * Root (when enabled) is emitted FIRST as `""`; blank rows are dropped;
 * non-root entries are trimmed and de-duped, order-preserving.
 */
export function serializeRowsArray(rows: PrefixRow[], includeRoot: boolean): string[] {
  const out: string[] = includeRoot ? [''] : [];
  const seen = new Set<string>(out);
  for (const row of rows) {
    const text = row.text.trim();
    if (!text || seen.has(text)) continue;
    seen.add(text);
    out.push(text);
  }
  return out;
}
