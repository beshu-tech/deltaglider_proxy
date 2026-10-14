import assert from 'node:assert/strict';
import { test } from 'vitest';
import {
  buildBucketPayload,
  isAllDefaultRow,
  policyToRow,
  DEFAULT_ROW_FIELDS,
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
  // "Reset to defaults" spreads the defaults over the row.
  const reset: BucketPolicyRow = { ...row, ...DEFAULT_ROW_FIELDS };
  assert.equal(isAllDefaultRow(reset), false);
  for (const r of [cleared, reset]) {
    const res = buildBucketPayload([r], ['db-archive']);
    assert.ok(res.ok);
    const patch = res.body.buckets['db-archive'];
    assert.notEqual(patch, null);
    assert.equal(patch?.backend, null, 'the modeled field is cleared');
  }
});

test('a removed row whose server policy has unmodeled fields is cleared, not deleted', () => {
  const res = buildBucketPayload([], ['db-archive'], ['db-archive']);
  assert.ok(res.ok);
  const patch = res.body.buckets['db-archive'];
  assert.notEqual(patch, null);
  assert.equal(patch?.compression, null);
  assert.equal(patch?.backend, null);
});
