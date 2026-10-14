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
  assert.equal(v.label, 'in sync · 11 checked');
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
  assert.equal(v.label, '5 failed');
});

test('mixed: errors own the dot, proportions honest', () => {
  const v = m({ scanned: 100, copied: 75, errors: 25, status: 'succeeded' });
  assert.equal(v.state, 'mixed');
  assert.equal(v.dot, 'red', 'errors own the attention dot');
  assert.equal(v.greenPct, 75);
  assert.equal(v.redPct, 25);
  assert.equal(v.label, '75 copied · 25 failed');
});

test('failed with nothing acted on → loud red', () => {
  const v = m({ scanned: 58, errors: 0, copied: 0, skipped: 0, status: 'failed' });
  assert.equal(v.state, 'errors');
  assert.equal(v.redPct, 100);
  assert.equal(v.label, 'failed');
});

test('failed but with a real error count → uses the error label/proportions', () => {
  const v = m({ scanned: 58, errors: 58, status: 'failed' });
  assert.equal(v.label, '58 failed');
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

// Regression: queued and cancelling one-offs read "running · 0 copied…".
test('queued waits, cancelling says so — neither reads as running', () => {
  const q = m({ status: 'queued' });
  assert.equal(q.state, 'waiting');
  assert.equal(q.label, 'waiting to start');
  assert.equal(q.dot, 'muted');
  const c = m({ status: 'cancelling', copied: 20_001, scanned: 20_001 });
  assert.equal(c.state, 'cancelling');
  assert.equal(c.label, 'cancelling · 20,001 copied');
  assert.equal(m({ status: 'cancelling' }).label, 'cancelling');
});

test('cancelled → calm muted', () => {
  const v = m({ scanned: 10, copied: 3, status: 'cancelled' });
  assert.equal(v.state, 'idle');
  assert.equal(v.dot, 'muted');
  assert.equal(v.label, 'cancelled · 3 copied', 'says how far the run got');
  assert.equal(m({ status: 'cancelled' }).label, 'cancelled');
  assert.equal(m({ status: 'paused', copied: 2, scanned: 2 }).label, 'stopped · 2 copied', 'a pause keeps the cursor: not "cancelled"');
  assert.equal(m({ status: 'deferred', copied: 2, scanned: 2 }).label, 'deferred · 2 copied', 'the proxy stopped it: not "cancelled"');
});

test('huge numbers → thousands separators', () => {
  const v = m({ scanned: 4_000_000, copied: 3_883_000, errors: 58, status: 'succeeded' });
  assert.equal(v.label, '3,883,000 copied · 58 failed');
});

test('aria always carries the full breakdown', () => {
  const v = m({ scanned: 8, copied: 8, status: 'succeeded' });
  assert.match(v.aria, /8 scanned, 8 copied, 0 already in sync/);
});

// Regression: a run that deleted 1 orphan read "in sync".
test('replication deletes count as work', () => {
  const v = m({ scanned: 10, skipped: 10, deleted: 1, status: 'succeeded' });
  assert.equal(v.state, 'copied');
  assert.equal(v.label, '1 deleted · 10 already in sync');
});

// Regression: every kind used replication words ("straight copy", "in sync").
test('words per kind', () => {
  assert.equal(m({ kind: 'backfill-metadata', scanned: 4003, copied: 4003 }).label, '4,003 stamped');
  assert.equal(
    m({ kind: 'backfill-metadata', scanned: 7, skipped: 7 }).label,
    'all already stamped · 7 checked',
  );
  assert.equal(
    m({ kind: 'reencrypt', scanned: 10_003, copied: 3, skipped: 10_000 }).label,
    '3 re-encrypted · 10,000 already current',
  );
  assert.equal(m({ kind: 'lifecycle', verb: 'delete', scanned: 8000, copied: 8000 }).label, '8,000 deleted');
  assert.equal(m({ kind: 'lifecycle', verb: 'copy', scanned: 3, copied: 3 }).label, '3 copied');
  assert.equal(m({ kind: 'lifecycle', verb: 'move', scanned: 3, copied: 3 }).label, '3 moved');
  assert.equal(m({ kind: 'lifecycle', scanned: 40, skipped: 40 }).label, 'nothing due · 40 checked');
  // Lifecycle `processed` already holds the deletes of a delete rule.
  assert.equal(m({ kind: 'lifecycle', verb: 'delete', scanned: 5, copied: 5, deleted: 5 }).label, '5 deleted');
  for (const kind of ['backfill-metadata', 'reencrypt', 'migrate', 'lifecycle']) {
    assert.doesNotMatch(m({ kind, scanned: 3, skipped: 3 }).label, /in sync/, kind);
  }
});

test('running: skipped objects show, phase words for migrate', () => {
  // Regression: 3,996 skipped in 1.5 s while the row said "0 copied…".
  assert.equal(
    m({ kind: 'backfill-metadata', status: 'running', scanned: 3996, skipped: 3996 }).label,
    'running · 0 stamped · 3,996 already stamped…',
  );
  assert.equal(
    m({ kind: 'migrate', status: 'running', phase: 'verify', scanned: 4000, copied: 4000 }).label,
    'verifying the copies · 4,000 copied…',
  );
  assert.equal(m({ kind: 'migrate', status: 'running', phase: 'copy', copied: 5, scanned: 5 }).label, 'copying · 5 copied…');
  assert.equal(m({ kind: 'reencrypt', status: 'running', phase: 'counting' }).label, 'counting objects · 0 re-encrypted…');
});
