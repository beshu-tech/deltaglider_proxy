/** src/bucketPublic.ts: the public marker of the browser (B53). */
import assert from 'node:assert/strict';
import { test } from 'vitest';
import { publicScope, publicScopeText } from '../bucketPublic';

const cfg = (p: Record<string, { public?: boolean; public_prefixes?: string[] }>) => ({ bucket_policies: p });

test('the whole bucket: public: true or an empty prefix', () => {
  assert.deepEqual(publicScope(cfg({ media: { public: true } }), 'media'), { whole: true });
  assert.deepEqual(publicScope(cfg({ media: { public_prefixes: [''] } }), 'media'), { whole: true });
  // Policy keys are lower-case.
  assert.deepEqual(publicScope(cfg({ media: { public_prefixes: [''] } }), 'Media'), { whole: true });
});

test('some prefixes, none, or unknown config', () => {
  assert.deepEqual(publicScope(cfg({ downloads: { public_prefixes: ['pub/'] } }), 'downloads'), { whole: false, prefixes: ['pub/'] });
  assert.equal(publicScope(cfg({ releases: { public_prefixes: [] } }), 'releases'), null);
  assert.equal(publicScope(cfg({}), 'releases'), null);
  assert.equal(publicScope(undefined, 'releases'), null);
  assert.match(publicScopeText({ whole: false, prefixes: ['pub/', 'docs/'] }), /under pub\/, docs\//);
});
