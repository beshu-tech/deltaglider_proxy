/** src/uploadQueueStore.ts */
import assert from 'node:assert/strict';
import { test } from 'vitest';
import { createUploadQueueStore } from '../uploadQueueStore';

interface Item {
  id: string;
  status: 'queued' | 'uploading' | 'success' | 'cancelled';
  n: number;
}

const items = (count: number): Item[] =>
  Array.from({ length: count }, (_, i) => ({ id: `f${i}`, status: 'queued' as const, n: 0 }));

test('progress events cost O(1) each: 100k events over 5000 files stay fast', () => {
  // Explore finding 7: every progress event rebuilt the whole queue
  // (prev.map) and re-rendered every row, so 300 small files took minutes.
  const store = createUploadQueueStore<Item>();
  store.add(items(5000));
  const t0 = performance.now();
  for (let e = 0; e < 100_000; e++) {
    store.update(`f${e % 5000}`, (it) => ({ ...it, n: it.n + 1 }));
    // One snapshot (one React state update) per 50 events: a frame's worth.
    if (e % 50 === 49) store.snapshot();
  }
  const ms = performance.now() - t0;
  assert.equal(store.snapshot()[4999].n, 20);
  assert.ok(ms < 1000, `100k events took ${ms.toFixed(0)} ms`);
});

test('a snapshot keeps the identity of unchanged items and of an unchanged queue', () => {
  const store = createUploadQueueStore<Item>();
  store.add(items(3));
  const a = store.snapshot();
  assert.equal(store.snapshot(), a, 'no change -> same array');
  store.update('f1', (it) => ({ ...it, n: 1 }));
  const b = store.snapshot();
  assert.notEqual(b, a);
  assert.equal(b[0], a[0]);
  assert.equal(b[2], a[2]);
  assert.notEqual(b[1], a[1]);
  assert.deepEqual(b.map((i) => i.id), ['f0', 'f1', 'f2'], 'order is kept');
});

test('nextQueued is FIFO, skips items that left the queued state, and sees requeued items', () => {
  const store = createUploadQueueStore<Item>();
  store.add(items(3));
  const isQueued = (it: Item) => it.status === 'queued';
  store.update('f0', (it) => ({ ...it, status: 'cancelled' }));
  assert.equal(store.nextQueued(isQueued)?.id, 'f1');
  store.update('f1', (it) => ({ ...it, status: 'uploading' }));
  assert.equal(store.nextQueued(isQueued)?.id, 'f2');
  store.update('f2', (it) => ({ ...it, status: 'success' }));
  assert.equal(store.nextQueued(isQueued), undefined);
  store.update('f0', (it) => ({ ...it, status: 'queued' }));
  store.requeue('f0');
  assert.equal(store.nextQueued(isQueued)?.id, 'f0');
});

test('remove drops items and later updates to them are ignored', () => {
  const store = createUploadQueueStore<Item>();
  store.add(items(4));
  store.remove((it) => it.id === 'f1' || it.id === 'f3');
  assert.deepEqual(store.snapshot().map((i) => i.id), ['f0', 'f2']);
  store.update('f1', (it) => ({ ...it, n: 9 }));
  assert.deepEqual(store.snapshot().map((i) => i.id), ['f0', 'f2']);
  assert.equal(store.get('f2')?.id, 'f2');
});
