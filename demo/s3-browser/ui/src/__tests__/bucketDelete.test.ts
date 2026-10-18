import assert from 'node:assert/strict';
import { test } from 'vitest';
import {
  bucketDeleteRefusalText,
  classifyBucketDeleteRefusal,
  deleteConfirmText,
  deleteMenuEntry,
} from '../bucketDelete';

test('deleteMenuEntry', () => {
  // Menu entry.
  assert.deepEqual(deleteMenuEntry(undefined), { label: 'Delete bucket (checking contents…)', disabled: true });
  assert.deepEqual(deleteMenuEntry({ state: 'error' }), { label: 'Delete bucket…', disabled: false });
  assert.deepEqual(deleteMenuEntry({ state: 'known', count: 0, truncated: false }), { label: 'Delete bucket…', disabled: false });
  assert.equal(deleteMenuEntry({ state: 'known', count: 1, truncated: false }).label, 'Delete bucket (not empty: 1 object)');
  assert.equal(deleteMenuEntry({ state: 'known', count: 100, truncated: true }).label, 'Delete bucket (not empty: 100+ objects)');
});

test('deleteConfirmText', () => {
  // The confirm says "empty" only when the probe answered 0 objects.
  assert.match(deleteConfirmText({ state: 'known', count: 0, truncated: false }), /is empty/);
  for (const p of [undefined, { state: 'error' as const }, { state: 'checking' as const }]) {
    assert.doesNotMatch(deleteConfirmText(p), /is empty/, `probe ${JSON.stringify(p)}`);
  }
});

// The server's two BucketNotEmpty texts (src/s3_adapter_s3s/bucket.rs). The
// sidebar once read `multipart_uploads=N` from the first one as "the bucket
// looks empty but has pending uploads" and offered to abort every upload:
// the uploads of other users, while objects still blocked the delete.
const OBJECTS =
  'releases (blocked: visible object remains, example_key=builds/a, b.zip, multipart_uploads=2; action: delete user objects first)';
const FINALISING = 'releases (blocked: 1 multipart upload(s) finalising; retry in a few seconds)';

test('classifyBucketDeleteRefusal reads the server refusals', () => {
  const table: [string | undefined, string, ReturnType<typeof classifyBucketDeleteRefusal>][] = [
    ['BucketNotEmpty', OBJECTS, { kind: 'objects-remain', exampleKey: 'builds/a, b.zip', uploads: 2 }],
    [
      'BucketNotEmpty',
      'releases (blocked: visible object remains, example_key=x, multipart_uploads=0; action: delete user objects first)',
      { kind: 'objects-remain', exampleKey: 'x', uploads: 0 },
    ],
    ['BucketNotEmpty', FINALISING, { kind: 'finalising', uploads: 1 }],
    // The code only in the message text (the sidebar's formatted error).
    [undefined, `Delete bucket releases failed (409) [BucketNotEmpty]: ${FINALISING}`, { kind: 'finalising', uploads: 1 }],
    ['BucketNotEmpty', 'The bucket you tried to delete is not empty', { kind: 'not-empty' }],
    ['AccessDenied', 'Access Denied', null],
  ];
  for (const [code, message, want] of table) {
    assert.deepEqual(classifyBucketDeleteRefusal(code, message), want, message);
  }
});

test('bucketDeleteRefusalText never offers to abort uploads', () => {
  const objects = bucketDeleteRefusalText('releases', { kind: 'objects-remain', exampleKey: 'v1.zip', uploads: 2 });
  assert.equal(
    objects,
    'Bucket "releases" still holds objects, for example "v1.zip". Delete the objects first, and then delete the bucket. 2 multipart uploads are also in progress in it.',
  );
  assert.equal(
    bucketDeleteRefusalText('releases', { kind: 'finalising', uploads: 1 }),
    '1 multipart upload is finishing in bucket "releases". Try to delete the bucket again in a few seconds.',
  );
  assert.match(bucketDeleteRefusalText('releases', { kind: 'not-empty' }), /is not empty/);
  for (const r of [
    { kind: 'objects-remain' as const, uploads: 3 },
    { kind: 'finalising' as const, uploads: 3 },
  ]) {
    assert.doesNotMatch(bucketDeleteRefusalText('releases', r), /abort|looks empty/i);
  }
});
