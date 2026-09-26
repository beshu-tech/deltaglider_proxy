// Regression test for deriveMeter (the pure decision fn behind OutcomeMeter,
// living in jobsView.ts alongside the other job-display helpers).
import assert from 'node:assert/strict';
import { test } from 'vitest';
import { deriveMeter, type OutcomeMeterInput } from '../jobsView';

const base: OutcomeMeterInput = { scanned: 0, copied: 0, errors: 0, skipped: 0, status: 'succeeded', percent: null };
const m = (over: Partial<OutcomeMeterInput>) => deriveMeter({ ...base, ...over });

test('in-sync: everything skipped (the dominant, calm case) — NOT saturated green', () => {
  const v = m({ scanned: 11, skipped: 11, status: 'succeeded' });
  assert.equal(v.state, 'in-sync');
  assert.equal(v.dot, 'muted');
  assert.equal(v.label, 'in sync');
  assert.equal(v.greenPct, 0, 'in-sync must not paint green');
});

test('empty: nothing scanned at all', () => {
  const v = m({ scanned: 0, status: 'succeeded' });
  assert.equal(v.state, 'in-sync');
  assert.equal(v.label, 'no objects');
});

test('copied only', () => {
  const v = m({ scanned: 8, copied: 8, skipped: 0, status: 'succeeded' });
  assert.equal(v.state, 'copied');
  assert.equal(v.dot, 'green');
  assert.equal(v.greenPct, 100);
  assert.equal(v.redPct, 0);
  assert.equal(v.label, '8 copied');
});

test('errors only', () => {
  const v = m({ scanned: 5, errors: 5, status: 'succeeded' });
  assert.equal(v.state, 'errors');
  assert.equal(v.dot, 'red');
  assert.equal(v.redPct, 100);
  assert.equal(v.label, '5 errors');
  assert.equal(m({ scanned: 1, errors: 1, status: 'succeeded' }).label, '1 error', 'singular');
});

test('mixed: errors own the dot, proportions honest', () => {
  const v = m({ scanned: 100, copied: 75, errors: 25, status: 'succeeded' });
  assert.equal(v.state, 'mixed');
  assert.equal(v.dot, 'red', 'errors own the attention dot');
  assert.equal(v.greenPct, 75);
  assert.equal(v.redPct, 25);
  assert.equal(v.label, '75 copied · 25 err');
});

test('failed with nothing acted on → loud red', () => {
  const v = m({ scanned: 58, errors: 0, copied: 0, skipped: 0, status: 'failed' });
  assert.equal(v.state, 'errors');
  assert.equal(v.redPct, 100);
  assert.equal(v.label, 'failed');
});

test('failed but with a real error count → uses the error label/proportions', () => {
  const v = m({ scanned: 58, errors: 58, status: 'failed' });
  assert.equal(v.label, '58 errors');
});

test('running, known percent → green fill + amber dot', () => {
  const v = m({ scanned: 50, copied: 20, status: 'running', percent: 40 });
  assert.equal(v.state, 'running');
  assert.equal(v.dot, 'amber');
  assert.equal(v.greenPct, 40);
  assert.equal(v.label, 'running · 20 copied');
});

test('running, unknown percent → indeterminate (green 0), trailing ellipsis label', () => {
  const v = m({ scanned: 50, copied: 20, status: 'running', percent: null });
  assert.equal(v.state, 'running');
  assert.equal(v.greenPct, 0);
  assert.equal(v.label, 'running · 20 copied…');
});

test('queued / cancelling also read as running', () => {
  assert.equal(m({ status: 'queued' }).state, 'running');
  assert.equal(m({ status: 'cancelling' }).state, 'running');
});

test('cancelled → calm muted', () => {
  const v = m({ scanned: 10, copied: 3, status: 'cancelled' });
  assert.equal(v.state, 'idle');
  assert.equal(v.dot, 'muted');
  assert.equal(v.label, 'cancelled');
});

test('huge numbers → thousands separators', () => {
  const v = m({ scanned: 4_000_000, copied: 3_883_000, errors: 58, status: 'succeeded' });
  assert.equal(v.label, '3,883,000 copied · 58 err');
});

test('aria always carries the full breakdown', () => {
  const v = m({ scanned: 8, copied: 8, status: 'succeeded' });
  assert.match(v.aria, /8 scanned, 8 copied, 0 skipped/);
});
