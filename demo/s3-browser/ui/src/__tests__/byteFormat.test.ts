/**
 * UX harvest D25: one screen said "58.6 KiB / 100.0 KiB" in the quota refusal
 * and "58.6 KB / 175.8 KB" in the list, for the same 1024-based values. The UI
 * has ONE byte convention: binary units with IEC labels (KiB, MiB, GiB), from
 * ONE formatter, `formatBytes` in utils.ts, because every server limit is
 * binary. This source test refuses a second formatter and decimal labels.
 *
 * Exempt: utils.ts (the formatter) and validation/validators.ts (the parser
 * accepts what an operator types, "512MB" included).
 */
import { readdirSync, readFileSync } from 'node:fs';
import { join, relative } from 'node:path';
import { expect, test } from 'vitest';

const SRC = join(__dirname, '..');
const EXEMPT = new Set(['utils.ts', 'validation/validators.ts']);

function sourceFiles(dir: string): string[] {
  return readdirSync(dir, { withFileTypes: true }).flatMap((e) => {
    const p = join(dir, e.name);
    if (e.isDirectory()) return e.name === '__tests__' || e.name === 'test' ? [] : sourceFiles(p);
    return /\.tsx?$/.test(e.name) && !/\.test\.tsx?$/.test(e.name) ? [p] : [];
  });
}

/** Source without comments, so prose about "multi-GB files" is not a label. */
function stripComments(src: string): string {
  return src.replace(/\/\*[\s\S]*?\*\//g, '').replace(/(^|[^:'"`])\/\/.*$/gm, '$1');
}

const RULES = {
  /** A decimal unit shown to the user: "12 MB", '/GB', suffix="MB". */
  decimalLabel: (src: string) =>
    [...stripComments(src).matchAll(/(?:\d|['"`/(\s≥≈$])(KB|MB|GB|TB|PB)\b/g)].map((m) => m[0].trim()),
  /** A second byte formatter: its own 1024 log/pow, or a format…Bytes / format…Size function. */
  secondFormatter: (src: string) =>
    [
      ...stripComments(src).matchAll(
        /Math\.log\(1024\)|Math\.pow\(1024|\b(?:function|const|let)\s+(?:format|fmt|human|pretty)\w*(?:Bytes|Size)\b/g,
      ),
    ].map((m) => m[0]),
};

test('the rules catch what they are for', () => {
  expect(RULES.decimalLabel("`${n} MB`")).toHaveLength(1);
  expect(RULES.decimalLabel('<Input suffix="MB" />')).toHaveLength(1);
  expect(RULES.decimalLabel('at ${rate}/GB')).toHaveLength(1);
  expect(RULES.decimalLabel("`${n} MiB` // a 2 GB file")).toHaveLength(0);
  expect(RULES.secondFormatter('function formatSize(n: number) {')).toHaveLength(1);
  expect(RULES.secondFormatter('const i = Math.floor(Math.log(n) / Math.log(1024));')).toHaveLength(1);
  expect(RULES.secondFormatter("import { formatBytes } from '../utils';")).toHaveLength(0);
});

for (const rule of Object.keys(RULES) as Array<keyof typeof RULES>) {
  test(`${rule}: no file outside utils.ts`, () => {
    const found: string[] = [];
    for (const file of sourceFiles(SRC)) {
      const rel = relative(SRC, file);
      if (EXEMPT.has(rel)) continue;
      const hits = RULES[rule](readFileSync(file, 'utf8'));
      if (hits.length > 0) found.push(`${rel}: ${hits.join(' | ')} — use formatBytes (utils.ts) and IEC labels`);
    }
    expect(found).toEqual([]);
  });
}
