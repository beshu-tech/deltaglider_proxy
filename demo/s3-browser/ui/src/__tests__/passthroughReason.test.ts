import assert from 'node:assert/strict';
import { test } from 'vitest';
import { bucketDeltaExtensions, isDeltaEligible, passthroughReason, passthroughSentence } from '../passthroughReason';

const DEFAULTS = ['zip', 'bin', 'sql'];

test('the bucket list is the built-in list plus add, minus remove', () => {
  assert.deepEqual(bucketDeltaExtensions(DEFAULTS, { delta_extensions: { add: ['tar.zst'], remove: ['sql'] } }), ['zip', 'bin', 'tar.zst']);
  assert.ok(isDeltaEligible('a/B.ZIP', DEFAULTS));
  assert.ok(!isDeltaEligible('a/b.zip.sha1', DEFAULTS));
});

test('passthrough reasons, in order of precedence', () => {
  assert.deepEqual(passthroughReason('x.zip', DEFAULTS, { compression: false }), { kind: 'compression-off' });
  assert.deepEqual(passthroughReason('x.zip', undefined, null), { kind: 'unknown' });
  assert.deepEqual(passthroughReason('fw/x.zip', DEFAULTS, null), { kind: 'delta-not-kept' });
  assert.deepEqual(passthroughReason('img/x.PNG', DEFAULTS, null), { kind: 'file-type', ext: 'png' });
  assert.deepEqual(passthroughReason('v1.2/README', DEFAULTS, null), { kind: 'file-type', ext: null });
  assert.deepEqual(passthroughReason('d.sql', DEFAULTS, { delta_extensions: { remove: ['sql'] } }), { kind: 'file-type', ext: 'sql' });
  assert.match(passthroughSentence({ kind: 'file-type', ext: 'png' }), /\.png files/);
});
