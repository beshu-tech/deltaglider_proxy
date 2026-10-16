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
  runTitle,
  rulesTouchingBucket,
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

test('the Job cell title tells one run from the next', () => {
  const r = run({ id: 'replication:nightly#4', job_id: 'replication:nightly', kind: 'replication', run_id: 4 });
  assert.equal(runName(r), 'nightly');
  assert.equal(runTitle(r), 'nightly #4');
  assert.equal(runSubject(r), '');
  // A delete is named by what it removes, not by its bucket.
  assert.equal(runName(run({})), 'beshu');
  assert.equal(runTitle(run({})), 'beshu');
  assert.equal(runTitle(run({ detail: { prefixes: ['ror/e2e/'], objects: 0 } })), 'ror/e2e/');
  assert.equal(runSubject(run({ detail: { prefixes: ['ror/e2e/'], objects: 0 } })), '');
  const many = run({ detail: { prefixes: ['a/', 'b/', 'c/'], objects: 2 } });
  assert.equal(runTitle(many), 'a/ +3');
  assert.equal(runSubject(many), 'also b/, c/ · 2 selected objects');
  assert.equal(runTitle(run({ detail: { prefixes: [], objects: 1 } })), '1 selected object');
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

test('rulesTouchingBucket names each rule with its role on the bucket', () => {
  const row = (id: string, kind: string, scope: { bucket: string; target?: string }) =>
    run({ id, job_id: id, kind, name: id.split(':')[1], scope });
  const rows = [
    row('replication:dr', 'replication', { bucket: 'releases', target: 'db-archive' }),
    row('lifecycle:expire', 'lifecycle', { bucket: 'db-archive' }),
    row('lifecycle:archive', 'lifecycle', { bucket: 'downloads', target: 'db-archive' }),
    row('maintenance:3', 'delete', { bucket: 'db-archive' }),
  ];
  assert.deepEqual(
    rulesTouchingBucket(rows, 'db-archive').map((r) => `${r.name}:${r.role}`),
    ['archive:archive', 'expire:bucket', 'dr:destination'],
  );
  assert.deepEqual(rulesTouchingBucket(rows, 'releases').map((r) => r.role), ['source']);
});
