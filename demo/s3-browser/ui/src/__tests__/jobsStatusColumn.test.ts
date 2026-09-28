/**
 * A running one-off shows an OutcomeMeter in the Status column: dot, track
 * (min 28px) and a label such as "running · 1,234 copied" (about 130px at
 * 11px). With a zero minimum the column shrank to about 125px on a 1280px
 * wide page, the meter's container query dropped the track, and the label
 * read "run…". jsdom has no layout, so this pins the column minimum.
 */
import { readFileSync } from 'node:fs';
import { expect, test } from 'vitest';

test('the Jobs Status column is wide enough for a running meter', () => {
  const s = readFileSync(new URL('../components/jobs/JobsPanel.tsx', import.meta.url), 'utf8');
  const m = /key: 'status',[\s\S]{0,240}track: 'minmax\((\d+)px/.exec(s);
  expect(m, 'the status column has a px minimum').not.toBeNull();
  expect(Number(m![1])).toBeGreaterThanOrEqual(200);
});
