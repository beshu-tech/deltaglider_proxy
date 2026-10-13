/**
 * Pure: turn the server's per-path diff into lines a reader can scan.
 *
 * The server diffs a section by path, so a change inside one rule of
 * `replication.rules` arrives as the WHOLE array before and after: two long
 * lines that look the same once truncated. An array of named objects (rules)
 * is split here into one line per added or removed rule and one line per
 * changed field of a kept rule.
 */

export interface DiffChange {
  before: unknown;
  after: unknown;
}

export interface DiffLine {
  path: string;
  /** 'added' / 'removed' show one side only; 'changed' shows both. */
  kind: 'added' | 'removed' | 'changed';
  before?: unknown;
  after?: unknown;
}

type Named = { name: string } & Record<string, unknown>;

function isPlainObject(v: unknown): v is Record<string, unknown> {
  return v !== null && typeof v === 'object' && !Array.isArray(v);
}

function isNamedArray(v: unknown): v is Named[] {
  return Array.isArray(v) && v.every((x) => isPlainObject(x) && typeof x.name === 'string');
}

function stable(v: unknown): string {
  return JSON.stringify(v, (_k, x) => {
    if (!isPlainObject(x)) return x;
    const o: Record<string, unknown> = {};
    for (const k of Object.keys(x).sort()) o[k] = x[k];
    return o;
  }) ?? 'undefined';
}

/** Changed leaves between two objects; plain objects are walked, arrays compared whole. */
function fieldChanges(path: string, before: unknown, after: unknown, out: DiffLine[]) {
  if (stable(before) === stable(after)) return;
  if (isPlainObject(before) && isPlainObject(after)) {
    const keys = [...new Set([...Object.keys(before), ...Object.keys(after)])].sort();
    for (const k of keys) fieldChanges(`${path}.${k}`, before[k], after[k], out);
    return;
  }
  out.push({ path, kind: 'changed', before, after });
}

export function expandDiff(rows: Array<[string, DiffChange]>): DiffLine[] {
  const out: DiffLine[] = [];
  for (const [path, change] of rows) {
    const before = change.before ?? [];
    const after = change.after ?? [];
    if (!isNamedArray(before) || !isNamedArray(after) || (before.length === 0 && after.length === 0)) {
      out.push({ path, kind: 'changed', before: change.before, after: change.after });
      continue;
    }
    const start = out.length;
    const was = new Map(before.map((r) => [r.name, r]));
    const now = new Map(after.map((r) => [r.name, r]));
    for (const r of after) {
      const old = was.get(r.name);
      if (!old) out.push({ path: `${path}["${r.name}"]`, kind: 'added', after: r });
      else fieldChanges(`${path}["${r.name}"]`, old, r, out);
    }
    for (const r of before) {
      if (!now.has(r.name)) out.push({ path: `${path}["${r.name}"]`, kind: 'removed', before: r });
    }
    // Same rules, other order: the order is part of the config.
    if (out.length === start && stable(before) !== stable(after)) {
      out.push({ path, kind: 'changed', before: before.map((r) => r.name), after: after.map((r) => r.name) });
    }
  }
  return out;
}
