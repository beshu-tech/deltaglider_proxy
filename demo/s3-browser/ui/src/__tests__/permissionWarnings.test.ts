import assert from 'node:assert/strict';
import { test } from 'vitest';
import { unknownBucketWarnings, invalidPatternWarnings } from '../components/permissionWarnings';

const BUCKETS = ['beshu', 'debug', 'archive'];

test('unknownBucketWarnings: flags a resource whose bucket segment is not real', () => {
  // THE bug from the user's config: `ror/lib/*` parses to bucket `ror` which is
  // not a real bucket. Must warn, and suggest nothing absurd.
  const w = unknownBucketWarnings(['ror/lib/*'], BUCKETS);
  assert.equal(w.length, 1);
  assert.equal(w[0].bucket, 'ror');
  assert.equal(w[0].resource, 'ror/lib/*');
});

test('unknownBucketWarnings: no warning for correct / wildcard-only / bare-bucket / template patterns', () => {
  // A correct resource on a real bucket → no warning.
  assert.deepEqual(unknownBucketWarnings(['beshu/ror/libs/*'], BUCKETS), []);

  // Wildcard-only and bare bucket → no warning.
  assert.deepEqual(unknownBucketWarnings(['*'], BUCKETS), []);
  assert.deepEqual(unknownBucketWarnings(['beshu'], BUCKETS), []);
  assert.deepEqual(unknownBucketWarnings(['beshu/*'], BUCKETS), []);

  // Template bucket → skipped (can't validate `${...}`).
  assert.deepEqual(unknownBucketWarnings(['${iam:username}/scrap/*'], BUCKETS), []);
});

test('unknownBucketWarnings: mixed lists warn once per bad bucket, deduped', () => {
  // Mixed list: one good, one bad → exactly one warning for the bad one.
  {
    const w = unknownBucketWarnings(['beshu/ror/libs/*', 'ror/lib/*'], BUCKETS);
    assert.equal(w.length, 1);
    assert.equal(w[0].bucket, 'ror');
  }

  // Duplicate bad bucket across rows → de-duped to one warning.
  {
    const w = unknownBucketWarnings(['ror/a/*', 'ror/b/*'], BUCKETS);
    assert.equal(w.length, 1);
  }
});

test('unknownBucketWarnings: near-miss suggestion and empty known-bucket list', () => {
  // Near-miss suggestion: typo of a real bucket gets suggested.
  {
    const w = unknownBucketWarnings(['beshuu/x/*'], BUCKETS); // 1 extra char
    assert.equal(w.length, 1);
    assert.equal(w[0].suggestion, 'beshu');
  }

  // Empty known-bucket list (still loading) → no false positives.
  assert.deepEqual(unknownBucketWarnings(['ror/lib/*'], []), []);
});

test('invalidPatternWarnings: valid patterns produce no warnings', () => {
  // Valid patterns → no warnings.
  assert.deepEqual(invalidPatternWarnings(['beshu/ror/libs/*']), []);
  assert.deepEqual(invalidPatternWarnings(['beshu', 'beshu/*', '*']), []);
  assert.deepEqual(invalidPatternWarnings(['beshu/my-bucket.name/*']), []); // hyphens/dots OK
  assert.deepEqual(invalidPatternWarnings(['${iam:username}/x/*']), []); // template OK here
});

test('invalidPatternWarnings: mid-pattern wildcard, whitespace, dedup', () => {
  // Mid-pattern `*` → rejected (only trailing allowed).
  {
    const w = invalidPatternWarnings(['beshu/*/thing']);
    assert.equal(w.length, 1);
    assert.ok(w[0].includes('mid-pattern'));
  }
  // Internal whitespace → rejected.
  {
    const w = invalidPatternWarnings(['beshu/a b/*']);
    assert.equal(w.length, 1);
    assert.ok(w[0].includes('space or control'));
  }
  // A wildcard inside the bucket segment → rejected as mid-pattern.
  assert.equal(invalidPatternWarnings(['b*cket/x']).length, 1);
  // Trailing whitespace alone is trimmed away → valid (no false positive).
  assert.deepEqual(invalidPatternWarnings(['beshu/ok/*  ']), []);
  // Multiple bad patterns → one message each, de-duped.
  {
    const w = invalidPatternWarnings(['a/*/b', 'a/*/b', 'c d/*']);
    assert.equal(w.length, 2);
  }
});
