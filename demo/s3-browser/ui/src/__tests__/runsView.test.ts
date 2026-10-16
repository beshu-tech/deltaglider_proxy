import assert from 'node:assert/strict';
import { test } from 'vitest';
import type { RunEntry } from '../adminApi';
import {
  runActions,
  runFiltersFromQuery,
  runFiltersToQuery,
  runName,
  runOriginLabel,
  runSubject,
} from '../runsView';

const run = (over: Partial<RunEntry>): RunEntry => ({
  id: 'maintenance:1',
  job_id: 'maintenance:1',
  kind: 'delete',
  name: 'beshu',
  scope: { bucket: 'beshu' },
  trigger: 'oneoff',
  status: 'succeeded',
  status_raw: 'completed',
  progress: { processed: 0, bytes: 0, failed: 0, skipped: 0 },
  detail: {},
  origin: 'admin',
  ...over,
});

test('filters round-trip through the URL query and drop unknown values', () => {
  const f = runFiltersFromQuery({ status: 'failed', kind: 'delete', bucket: 'b', noop: 'true', bogus: 'x' });
  assert.deepEqual(f, { status: 'failed', kind: 'delete', bucket: 'b', noop: true });
  assert.deepEqual(runFiltersToQuery(f, 'maintenance:3'), {
    status: 'failed',
    kind: 'delete',
    bucket: 'b',
    noop: 'true',
    job: 'maintenance:3',
  });
  assert.deepEqual(runFiltersFromQuery({ status: 'all' }), {});
});

test('the Job cell names the rule and the run, or what a delete removed', () => {
  const r = run({ id: 'replication:nightly#4', job_id: 'replication:nightly', kind: 'replication', run_id: 4 });
  assert.equal(runName(r), 'nightly');
  assert.equal(runSubject(r), 'run #4');
  assert.equal(runName(run({})), 'beshu');
  assert.equal(runSubject(run({ detail: { prefixes: ['ror/e2e/'], objects: 0 } })), 'ror/e2e/');
  assert.equal(
    runSubject(run({ detail: { prefixes: ['a/', 'b/', 'c/'], objects: 2 } })),
    'a/ and 2 more folders · 2 selected objects',
  );
});

test('origins name who started a one-off', () => {
  assert.equal(runOriginLabel({ origin: 'schedule' }), 'Schedule');
  assert.equal(runOriginLabel({ origin: 'file-browser', started_by: 'dana' }), 'File browser · dana');
  assert.equal(runOriginLabel({ origin: 'admin', started_by: 'admin' }), 'Admin');
});

test('actions: cancel or kill while live, remove a finished one-off', () => {
  assert.deepEqual(runActions({ kind: 'delete', status: 'running' }), ['cancel']);
  assert.deepEqual(runActions({ kind: 'delete', status: 'cancelling' }), []);
  assert.deepEqual(runActions({ kind: 'delete', status: 'failed' }), ['remove']);
  assert.deepEqual(runActions({ kind: 'replication', status: 'running' }), ['kill']);
  assert.deepEqual(runActions({ kind: 'lifecycle', status: 'succeeded' }), []);
});
