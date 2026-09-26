// Browser-review item 24: the bucket-name inputs silently rewrote what the
// user typed ("My_Bucket" became "mybucket"). They now keep the text and
// say what is wrong, with the server's rules (src/security.rs).
import assert from 'node:assert/strict';
import { test } from 'vitest';
import { bucketNameError } from '../bucketName';

test('valid names pass', () => {
  for (const n of ['releases', 'db-archive', 'a.b-c', 'x12']) assert.equal(bucketNameError(n), null, n);
});

test('each rule names itself', () => {
  assert.match(bucketNameError('My_Bucket')!, /lowercase letters, digits, dots and hyphens/);
  assert.match(bucketNameError('ab')!, /3 to 63 characters/);
  assert.match(bucketNameError('a'.repeat(64))!, /3 to 63 characters/);
  assert.match(bucketNameError('a..b')!, /two dots in a row/);
  assert.match(bucketNameError('-abc')!, /start and end with a letter or digit/);
  assert.match(bucketNameError('abc.')!, /start and end with a letter or digit/);
  assert.match(bucketNameError('192.168.1.1')!, /IP address/);
  assert.equal(bucketNameError(''), null); // empty: no message, the button is off
});
