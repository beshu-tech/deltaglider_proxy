/**
 * A running one-off shows an OutcomeMeter in the Status column. The meter is
 * size-contained (container-type: inline-size), so it has no intrinsic width,
 * and the RecordList cell is a flex row: a wrapper without `flex: 1` shrank to
 * nothing, the meter dropped its track and the label read "run…" (the
 * migrate-jobs-row screenshot). jsdom has no layout, so this pins the wrapper.
 */
import { readFileSync } from 'node:fs';
import { expect, test } from 'vitest';

test('the Jobs Status cell lets the meter fill the column', () => {
  const s = readFileSync(new URL('../components/jobs/JobsPanel.tsx', import.meta.url), 'utf8');
  const status = s.slice(s.indexOf("key: 'status',"), s.indexOf('<OutcomeMeter', s.indexOf("key: 'status',")));
  expect(status).toMatch(/<div style=\{\{[^}]*flex: 1[^}]*\}\}>/);
  expect(readFileSync(new URL('../components/jobs/RecordList.css', import.meta.url), 'utf8')).toMatch(
    /\.dg-record-cell \{[^}]*display: flex;/,
  );
});
