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

/** null and undefined are one thing in a diff: "not set". */
const unset = (v: unknown) => (v === null ? undefined : v);

/**
 * Pure: the lines of one change. Plain objects are walked key by key (also
 * when one side is not set: a section created or removed as a whole lists
 * its fields); an array of named objects (rules) splits into one line per
 * added or removed rule and one line per changed field of a kept rule;
 * anything else is one line.
 */
function walk(path: string, rawBefore: unknown, rawAfter: unknown, out: DiffLine[]) {
  const before = unset(rawBefore);
  const after = unset(rawAfter);
  if (stable(before) === stable(after)) return;
  const namedBefore = before === undefined ? [] : before;
  const namedAfter = after === undefined ? [] : after;
  if (isNamedArray(namedBefore) && isNamedArray(namedAfter) && (namedBefore.length > 0 || namedAfter.length > 0)) {
    const start = out.length;
    const was = new Map(namedBefore.map((r) => [r.name, r]));
    const now = new Map(namedAfter.map((r) => [r.name, r]));
    for (const r of namedAfter) {
      const old = was.get(r.name);
      if (!old) out.push({ path: `${path}["${r.name}"]`, kind: 'added', after: r });
      else walk(`${path}["${r.name}"]`, old, r, out);
    }
    for (const r of namedBefore) {
      if (!now.has(r.name)) out.push({ path: `${path}["${r.name}"]`, kind: 'removed', before: r });
    }
    // Same rules, other order: the order is part of the config.
    if (out.length === start) {
      out.push({ path, kind: 'changed', before: namedBefore.map((r) => r.name), after: namedAfter.map((r) => r.name) });
    }
    return;
  }
  const objectOrUnset = (v: unknown) => v === undefined || isPlainObject(v);
  if (objectOrUnset(before) && objectOrUnset(after)) {
    const b = (before ?? {}) as Record<string, unknown>;
    const a = (after ?? {}) as Record<string, unknown>;
    const keys = [...new Set([...Object.keys(b), ...Object.keys(a)])].sort();
    for (const k of keys) walk(path ? `${path}.${k}` : k, b[k], a[k], out);
    return;
  }
  if (before === undefined) out.push({ path, kind: 'added', after });
  else if (after === undefined) out.push({ path, kind: 'removed', before });
  else out.push({ path, kind: 'changed', before, after });
}

export function expandDiff(rows: Array<[string, DiffChange]>): DiffLine[] {
  const out: DiffLine[] = [];
  for (const [path, change] of rows) walk(path, change.before, change.after, out);
  return out;
}

/** One field of a value, for a field list: `source.bucket` → `"releases"`. */
export interface DiffField {
  key: string;
  value: string;
}

/** Pure: a scalar or a list of scalars as one short text; strings quoted. */
export function formatDiffValue(v: unknown): string {
  if (v === undefined || v === null) return 'not set';
  if (typeof v === 'string') return JSON.stringify(v);
  if (typeof v === 'number' || typeof v === 'boolean') return String(v);
  if (Array.isArray(v) && v.every((x) => !isPlainObject(x) && !Array.isArray(x))) {
    return v.length === 0 ? 'none' : v.map(formatDiffValue).join(', ');
  }
  return JSON.stringify(v);
}

/** Pure: a value as a flat field list (nested objects by dotted key); a
 *  scalar is null (it is shown on its own line). Never truncated. */
export function diffFields(v: unknown, prefix = ''): DiffField[] | null {
  if (!isPlainObject(v)) return null;
  const out: DiffField[] = [];
  for (const k of Object.keys(v)) {
    const key = prefix ? `${prefix}.${k}` : k;
    const nested = diffFields(v[k], key);
    if (nested && nested.length > 0) out.push(...nested);
    else out.push({ key, value: formatDiffValue(v[k]) });
  }
  return out;
}
