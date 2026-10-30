import assert from 'node:assert/strict';
import { test } from 'vitest';
import { bucketRowEdited, policyToRow, rebaseBucketRows, type BucketPolicyRow } from '../components/bucketPolicyPayload';

// UX harvest B26: an encryption change applies at once while other buckets
// have staged edits. The staged rows are rebased per bucket: an untouched
// bucket takes the server's new row, an edited one keeps the user's row.

type PolicyGet = Parameters<typeof policyToRow>[1];
const row = (name: string, p: PolicyGet): BucketPolicyRow => policyToRow(name, p);

const before = [row('releases', { quota_bytes: 10 }), row('media', { public: true } as PolicyGet)];
const next = [
  row('db-archive', { backend: 'hetzner-fsn1', encryption: { mode: 'aes256-gcm-proxy', key_id: 'k1' } } as PolicyGet),
  row('releases', { quota_bytes: 10 }),
  row('media', { public: true } as PolicyGet),
];

test('with no staged edits the rebase is the server value', () => {
  const cur = before.map((r) => ({ ...r, _id: `x-${r.name}` }));
  assert.deepEqual(rebaseBucketRows(cur, before, next), next);
});

test('an edited bucket keeps its staged row; the changed bucket takes the server row', () => {
  const edited = { ...before[0], quota_bytes: 20 };
  const out = rebaseBucketRows([edited, before[1]], before, next);
  assert.deepEqual(out.map((r) => r.name), ['db-archive', 'releases', 'media']);
  assert.equal(out[0], next[0]);
  assert.equal(out[1], edited);
  assert.equal(out[2], next[2]);
});

test('a row the user removed stays removed; a draft stays', () => {
  const draft = { ...before[0], _id: 'd', name: '' };
  const out = rebaseBucketRows([before[0], draft], before, next);
  assert.deepEqual(out.map((r) => r.name), ['db-archive', 'releases', '']);
});

test('bucketRowEdited compares settings, not the client id', () => {
  assert.equal(bucketRowEdited([{ ...before[0], _id: 'other' }], before, 'releases'), false);
  assert.equal(bucketRowEdited([{ ...before[0], quota_bytes: 1 }], before, 'releases'), true);
  assert.equal(bucketRowEdited([], before, 'releases'), true);
  assert.equal(bucketRowEdited([], before, 'downloads'), false);
});
