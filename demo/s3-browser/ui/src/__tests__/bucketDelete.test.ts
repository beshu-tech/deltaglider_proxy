import assert from 'node:assert/strict';
import { test } from 'vitest';
import { deleteMenuEntry, deleteConfirmText } from '../bucketDelete';

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
