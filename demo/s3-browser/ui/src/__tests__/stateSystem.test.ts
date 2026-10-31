/**
 * THE state system and THE error layer (UX harvest D16, D17, D24, D35):
 * empty, error and loading states come from components/StatePlaceholders
 * (EmptyState / ErrorState / LoadingState / QueryState), and an error
 * reaches the screen through `plainError` (one sentence; the wire text in
 * "Details"). This source test refuses the shapes that bypass them:
 *
 *  1. the stock AntD <Empty> (a grey drawing and a sentence, an eighth
 *     empty style);
 *  2. a hand-written "No … yet" sentence that is not the title or hint of
 *     an EmptyState (a `title`/`hint` prop or key, a dashboard `noData(…)`);
 *  3. raw error text in a component: `normalizeUiError(…)` (it returns the
 *     wire text: status, code, request id) or `{err.message}`/`{error.message}`.
 *
 * RATCHET: files not yet migrated are listed with their current count. A
 * count may go down, never up; any other file must have none. Wave 2
 * empties the lists (~/.cache/claude-work/ux-harvest/cycle3/wave2-todo.md).
 */
import { readdirSync, readFileSync } from 'node:fs';
import { join, relative } from 'node:path';
import { expect, test } from 'vitest';

const SRC = join(__dirname, '..');
const OWNER = 'components/StatePlaceholders.tsx';

function uiFiles(dir: string): string[] {
  return readdirSync(dir, { withFileTypes: true }).flatMap((e) => {
    const p = join(dir, e.name);
    if (e.isDirectory()) return e.name === '__tests__' || e.name === 'test' ? [] : uiFiles(p);
    return /\.tsx$/.test(e.name) ? [p] : [];
  });
}

/** Pure: the offending lines of one rule in one source. */
const RULES = {
  antdEmpty: (src: string) =>
    src.split('\n').filter((l) => /import\s*\{[^}]*\bEmpty\b[^}]*\}\s*from\s*'antd'|<Empty\b/.test(l)),
  adHocNoYet: (src: string) =>
    src
      .split('\n')
      .filter((l) => /(['"`>]|\s)No [a-z][^'"`<>{}]{0,80}\byet\b/.test(l))
      .filter((l) => !/\b(title|hint|helpText)\s*[=:]|noData\(|^\s*(\/\/|\*)/.test(l)),
  rawErrorText: (src: string) =>
    src.split('\n').filter((l) => /normalizeUiError\(|\{\s*(err|error)\.message\s*\}/.test(l)),
};

/** file → most offending lines it may keep (not yet migrated). */
const ALLOWED: Record<keyof typeof RULES, Record<string, number>> = {
  antdEmpty: {},
  adHocNoYet: {},
  rawErrorText: {},
};

test('the rules catch what they are for', () => {
  expect(RULES.antdEmpty("import { Button, Empty } from 'antd';")).toHaveLength(1);
  expect(RULES.adHocNoYet("<Text>No runs yet. Rules run on their schedule.</Text>")).toHaveLength(1);
  expect(RULES.adHocNoYet("<EmptyState title=\"No runs yet\" />")).toHaveLength(0);
  expect(RULES.rawErrorText('setError(normalizeUiError(e, "x"));')).toHaveLength(1);
  expect(RULES.rawErrorText('<span>{e.message}</span>')).toHaveLength(0); // a log line, not an error
});

for (const rule of Object.keys(RULES) as Array<keyof typeof RULES>) {
  test(`${rule}: only the allow-listed files, and never more than their count`, () => {
    const over: string[] = [];
    const counts: Record<string, number> = {};
    for (const file of uiFiles(SRC)) {
      const rel = relative(SRC, file);
      if (rel === OWNER) continue;
      const n = RULES[rule](readFileSync(file, 'utf8')).length;
      counts[rel] = n;
      const allowed = ALLOWED[rule][rel] ?? 0;
      if (n > allowed) over.push(`${rel}: ${n} (allowed ${allowed}) — use StatePlaceholders / plainError`);
    }
    expect(over).toEqual([]);
    const stale = Object.entries(ALLOWED[rule]).filter(([rel, n]) => (counts[rel] ?? 0) < n);
    expect(stale.map(([rel]) => `${rel}: lower its count`)).toEqual([]);
  });
}
