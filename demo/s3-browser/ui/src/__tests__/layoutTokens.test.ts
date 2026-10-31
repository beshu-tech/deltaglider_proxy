/**
 * UX harvest D22: card gaps of 8, 16, 24, 32, 40 and 48 px across the admin,
 * radius 6/8/12, padding 24 / 20 24 / 8 12. The spacing scale lives in
 * shared-styles.ts (SPACE: 4 8 12 16 24 32). This test refuses a numeric
 * margin / padding / gap off that scale in the components (1 and 2 px are
 * allowed for hairline nudges; clamp()/calc()/var()/% and template values
 * are not checked).
 *
 * ALLOWED is empty: every component is on the scale. Keep it that way; an
 * entry is a last resort and needs a reason in a comment.
 */
import { readdirSync, readFileSync } from 'node:fs';
import { join, relative } from 'node:path';
import { expect, test } from 'vitest';
import { SPACING_SCALE } from '../components/shared-styles';

const COMPONENTS = join(__dirname, '..', 'components');
const ALLOWED_PX = new Set([...SPACING_SCALE, 1, 2]);
const PROP =
  /\b(margin|padding|gap|rowGap|columnGap|(?:margin|padding)(?:Top|Bottom|Left|Right|Inline|Block|InlineStart|InlineEnd|BlockStart|BlockEnd))\s*:\s*(-?\d+(?:\.\d+)?|'[^'\n]*'|"[^"\n]*")/g;

/** Off-scale spacing values in one file's source. Pure. */
export function offScaleSpacing(source: string): string[] {
  const out: string[] = [];
  for (const m of source.matchAll(PROP)) {
    const value = m[2];
    let px: number[];
    if (/^-?\d/.test(value)) px = [Number(value)];
    else {
      if (/clamp|calc|var\(|%|\bem\b|rem|auto/.test(value)) continue;
      px = [...value.matchAll(/(-?\d+(?:\.\d+)?)px/g)].map((x) => Number(x[1]));
    }
    if (px.some((n) => !ALLOWED_PX.has(Math.abs(n)))) out.push(`${m[1]}: ${value}`);
  }
  return out;
}

function tsxFiles(dir: string): string[] {
  return readdirSync(dir, { withFileTypes: true }).flatMap((e) => {
    const p = join(dir, e.name);
    if (e.isDirectory()) return tsxFiles(p);
    return /\.tsx$/.test(e.name) ? [p] : [];
  });
}

/** Not yet on the scale: file → most off-scale values it may keep. */
const ALLOWED: Record<string, number> = {};

test('the scanner finds off-scale values and skips scale values', () => {
  expect(offScaleSpacing("padding: '12px 20px', gap: 8, margin: 0, marginTop: 2")).toEqual(["padding: '12px 20px'"]);
  expect(offScaleSpacing('gap: 10, padding: clamp(16px, 3vw, 24px)')).toEqual(['gap: 10']);
});

test('components use the spacing scale (ratchet)', () => {
  const over: string[] = [];
  for (const file of tsxFiles(COMPONENTS)) {
    const rel = relative(COMPONENTS, file);
    const found = offScaleSpacing(readFileSync(file, 'utf8'));
    const allowed = ALLOWED[rel] ?? 0;
    if (found.length > allowed) over.push(`${rel}: ${found.length} off-scale (allowed ${allowed}): ${found.slice(0, 5).join(' | ')}`);
  }
  expect(over).toEqual([]);
});

test('the allow-list has no stale entries', () => {
  const stale = Object.entries(ALLOWED).filter(([rel, n]) => {
    try {
      return offScaleSpacing(readFileSync(join(COMPONENTS, rel), 'utf8')).length < n;
    } catch {
      return true;
    }
  });
  expect(stale.map(([rel]) => `${rel}: lower its count`)).toEqual([]);
});
