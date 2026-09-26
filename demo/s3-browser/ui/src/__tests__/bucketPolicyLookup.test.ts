import assert from 'node:assert/strict';
import { test } from 'vitest';
import { bucketPolicyFor } from '../bucketPolicyLookup';

test('bucketPolicyFor: exact key wins, falls back to lower-case, else undefined', () => {
  const exact = { compression: false };
  const lower = { compression: true };
  const config = { bucket_policies: { Releases: exact, downloads: lower } };

  assert.equal(bucketPolicyFor(config, 'Releases'), exact, 'exact key wins');
  assert.equal(bucketPolicyFor(config, 'Downloads'), lower, 'falls back to the lower-cased key');
  assert.equal(bucketPolicyFor(config, 'downloads'), lower);
  assert.equal(bucketPolicyFor(config, 'db-archive'), undefined, 'unknown bucket → undefined');
  assert.equal(bucketPolicyFor(null, 'releases'), undefined, 'no config → undefined');
  assert.equal(bucketPolicyFor(undefined, 'releases'), undefined);
  assert.equal(bucketPolicyFor({ bucket_policies: {} }, 'releases'), undefined, 'no bucket_policies → undefined');
});
