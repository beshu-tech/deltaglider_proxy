/** src/pageTitle.ts (browser-tab titles). */
import assert from 'node:assert/strict';
import { test } from 'vitest';
import { pageTitle } from '../pageTitle';

test('page titles', () => {
  // A fresh load has no bucket yet: never a bare "— DeltaGlider Proxy".
  assert.equal(pageTitle('browser', ''), 'Browse — DeltaGlider Proxy');
  assert.equal(pageTitle('browser', 'releases'), 'releases — DeltaGlider Proxy');
  assert.equal(pageTitle('upload', 'releases'), 'Upload to releases — DeltaGlider Proxy');
  assert.equal(pageTitle('admin', '', 'Users'), 'Users · Settings — DeltaGlider Proxy');
  assert.equal(pageTitle('admin', 'releases'), 'Settings — DeltaGlider Proxy');
  for (const v of ['browser', 'upload', 'docs', 'admin'] as const) {
    assert.ok(!pageTitle(v, '').startsWith('—'), `${v}: title must not start with a dash`);
  }
});
