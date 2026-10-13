// TimeAgo re-renders on its own clock: "12s ago" moves between polls.
import { expect, test } from 'vitest';
import { tickPeriodMs } from '../hooks/useNow';

test('tickPeriodMs: every second while the text reads in seconds', () => {
  expect(tickPeriodMs(5)).toBe(1000);
  expect(tickPeriodMs(-5)).toBe(1000);
  expect(tickPeriodMs(600)).toBe(30_000);
  expect(tickPeriodMs(7200)).toBe(300_000);
});
