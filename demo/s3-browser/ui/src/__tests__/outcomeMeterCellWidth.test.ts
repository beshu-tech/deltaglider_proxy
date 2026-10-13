/**
 * An OutcomeMeter is size-contained (container-type: inline-size), so it has
 * no intrinsic width, and a RecordList cell is a flex row. A wrapper that is
 * not told to grow takes the width of its content: with nothing wider beside
 * the meter, the track dropped and the label read "run…" (Jobs Status) and
 * "in s…" (a Runs row without a strategy band). The fix had been made in one
 * call site and then the defect came back in the next, so one CSS rule now grows every
 * cell child that holds a meter. jsdom has no layout, so this pins the rule.
 */
import { readFileSync } from 'node:fs';
import { expect, test } from 'vitest';

const css = (name: string) =>
  readFileSync(new URL(`../components/jobs/${name}`, import.meta.url), 'utf8');

test('a RecordList cell that holds a meter grows its child to the column width', () => {
  expect(css('RecordList.css')).toMatch(/\.dg-record-cell \{[^}]*display: flex;/);
  expect(css('OutcomeMeter.css')).toMatch(
    /\.dg-record-cell:has\(\.dg-meter\) > \* \{[^}]*flex: 1 1 0;[^}]*min-width: 0;/,
  );
});
