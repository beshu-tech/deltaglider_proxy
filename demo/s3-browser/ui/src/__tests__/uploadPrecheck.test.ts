// Browser-review item 14: a 143 MB file over the 100 MB max_object_size
// uploaded 96 MB before the server refused it. The page checks size and
// remaining quota BEFORE any byte goes out.
import assert from 'node:assert/strict';
import { test } from 'vitest';
import { precheckUpload } from '../uploadPrecheck';

const MB = 1024 * 1024;
const f = (name: string, size: number) => ({ name, size });

test('no known limits: everything passes', () => {
  const r = precheckUpload([f('a', 10 * MB)], { maxObjectSize: null, quotaBytes: null, usedBytes: null });
  assert.equal(r.accepted.length, 1);
  assert.deepEqual(r.refused, []);
});

test('a file over max_object_size is refused with the limit named', () => {
  const r = precheckUpload([f('big.bin', 143 * MB), f('ok.bin', MB)], { maxObjectSize: 100 * MB, quotaBytes: null, usedBytes: null });
  assert.deepEqual(r.accepted.map((x) => x.name), ['ok.bin']);
  assert.equal(r.refused.length, 1);
  assert.equal(r.refused[0].file.name, 'big.bin');
  assert.match(r.refused[0].reason, /143\.0 MB.*100\.0 MB/);
});

test('the quota counts the used bytes and the files before it in the batch', () => {
  const r = precheckUpload([f('a', 30 * MB), f('b', 30 * MB), f('c', 10 * MB)], { maxObjectSize: null, quotaBytes: 100 * MB, usedBytes: 50 * MB });
  assert.deepEqual(r.accepted.map((x) => x.name), ['a', 'c']);
  assert.equal(r.refused[0].file.name, 'b');
  assert.match(r.refused[0].reason, /quota/);
});

test('an unknown usage never refuses on quota, and quota 0 is a frozen bucket', () => {
  assert.equal(precheckUpload([f('a', 999 * MB)], { maxObjectSize: null, quotaBytes: MB, usedBytes: null }).refused.length, 0);
  const frozen = precheckUpload([f('a', 1)], { maxObjectSize: null, quotaBytes: 0, usedBytes: null });
  assert.match(frozen.refused[0].reason, /frozen/);
});
