import assert from 'node:assert/strict';
import { test } from 'vitest';
import {
  buildBucketPayload,
  isAllDefaultRow,
  policyMustSurvive,
  policyToRow,
  resetRowFields,
  type BucketPolicyRow,
} from '../components/bucketPolicyPayload';

// Issue #116 review C: the Buckets panel does not model a bucket's
// `encryption` block (P1 is YAML only). A policy whose only field is
// `encryption` became an all-default row, and ANY apply (even an edit of
// another bucket) sent it as `null`, which deletes the policy and its key.

type PolicyGet = Parameters<typeof policyToRow>[1];

/** A policy as the storage-section GET returns it (key redacted). */
const encryptionOnly = {
  encryption: { mode: 'aes256-gcm-proxy', key_id: 'db-archive-2026' },
} as unknown as PolicyGet;

test('single backend: an encryption-only policy is not all-default and is never nulled', () => {
  const rows = [policyToRow('db-archive', encryptionOnly), policyToRow('releases', { compression: false })];
  assert.equal(isAllDefaultRow(rows[0]), false);
  // An edit of ANOTHER bucket.
  rows[1] = { ...rows[1], compression: true };
  const res = buildBucketPayload(rows, ['db-archive', 'releases']);
  assert.ok(res.ok);
  assert.notEqual(res.body.buckets['db-archive'], null, 'the encryption policy must not be deleted');
  // Untouched modeled fields go out as null; `encryption` is not sent, so
  // the merge-patch keeps it.
  const patch = res.body.buckets['db-archive'] as unknown as Record<string, unknown> | undefined;
  if (patch) assert.equal('encryption' in patch, false);
});

test('named backends: clearing `backend` keeps an encryption policy', () => {
  const row = policyToRow('db-archive', {
    backend: 'local-disk',
    encryption: { mode: 'none', legacy_key_id: 'db-archive-2025' },
  } as unknown as PolicyGet);
  const cleared: BucketPolicyRow = { ...row, backend: '' };
  assert.equal(isAllDefaultRow(cleared), false);
  const res = buildBucketPayload([cleared], ['db-archive']);
  assert.ok(res.ok);
  assert.notEqual(res.body.buckets['db-archive'], null);
});

// P2: "Reset to defaults" keeps the bucket's own encryption AND the backend
// route that it needs (the server refuses a block without `backend:` on
// named backends, and refuses to drop a key in use).
test('reset to defaults keeps the encryption block and its backend', () => {
  const row = policyToRow('db-archive', {
    backend: 'local-disk',
    quota_bytes: 5,
    encryption: { mode: 'aes256-gcm-proxy', key_id: 'db-archive-2026' },
  } as unknown as PolicyGet);
  const reset: BucketPolicyRow = { ...row, ...resetRowFields(row) };
  assert.equal(isAllDefaultRow(reset), false);
  assert.deepEqual(reset.encryption, { mode: 'aes256-gcm-proxy', key_id: 'db-archive-2026' });
  const res = buildBucketPayload([reset], ['db-archive']);
  assert.ok(res.ok);
  const patch = res.body.buckets['db-archive'];
  assert.equal(patch?.backend, 'local-disk');
  assert.equal(patch?.quota_bytes, null);
  assert.equal('encryption' in (patch ?? {}), false, 'the block is never sent by this panel');
  // Without a block, reset clears the route as before.
  const plain = policyToRow('releases', { backend: 'local-disk' } as unknown as PolicyGet);
  assert.equal(resetRowFields(plain).backend, '');
});

test('encryption is a modeled field: not in `unmodeled`, but the policy must survive', () => {
  const row = policyToRow('db-archive', encryptionOnly);
  assert.deepEqual(row.unmodeled, {});
  assert.notEqual(row.encryption, null);
  assert.equal(policyMustSurvive(encryptionOnly), true);
  assert.equal(policyMustSurvive({ compression: false }), false);
});

test('a removed row whose server policy has unmodeled fields is cleared, not deleted', () => {
  const res = buildBucketPayload([], ['db-archive'], ['db-archive']);
  assert.ok(res.ok);
  const patch = res.body.buckets['db-archive'];
  assert.notEqual(patch, null);
  assert.equal(patch?.compression, null);
  assert.equal(patch?.backend, null);
});
