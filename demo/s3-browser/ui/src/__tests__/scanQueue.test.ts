/** The bucket scan queue rules (review A10, C5). */
import assert from 'node:assert/strict';
import { test } from 'vitest';
import type { BucketScanProgress } from '../adminApi/bucketScan';
import { initialScanQueue, scanQueueReducer, type ScanQueueState } from '../scanQueue';

const frame = (bucket: string, over: Partial<BucketScanProgress> = {}): BucketScanProgress => ({
  bucket,
  objects: 1,
  original_bytes: 1,
  stored_bytes: 1,
  pages_done: 1,
  has_more: true,
  finished: false,
  error: null,
  started_at: '2026-10-09T00:00:00Z',
  ...over,
});
const queued = (...queue: string[]): ScanQueueState => ({ queue, notice: null });

test('an error frame on the head moves on and keeps the error as the notice', () => {
  const next = scanQueueReducer(queued('a', 'b'), {
    type: 'frame',
    frame: frame('a', { finished: true, error: 'injected EIO' }),
  });
  assert.deepEqual(next.queue, ['b']);
  assert.match(next.notice ?? '', /a.*injected EIO/);
});

test('an error frame without finished moves on too', () => {
  const next = scanQueueReducer(queued('a', 'b'), { type: 'frame', frame: frame('a', { error: 'x' }) });
  assert.deepEqual(next.queue, ['b']);
});

test('a finished frame moves on and keeps no notice', () => {
  const next = scanQueueReducer(queued('a', 'b'), { type: 'frame', frame: frame('a', { finished: true }) });
  assert.deepEqual(next, queued('b'));
});

test('a progress frame changes nothing', () => {
  const state = queued('a', 'b');
  assert.equal(scanQueueReducer(state, { type: 'frame', frame: frame('a') }), state);
});

test('a frame or a lost stream of a bucket that is not the head changes nothing', () => {
  const state = queued('b', 'c');
  assert.equal(
    scanQueueReducer(state, { type: 'frame', frame: frame('a', { finished: true, error: 'cancelled' }) }),
    state,
  );
  assert.equal(scanQueueReducer(state, { type: 'streamLost', bucket: 'a' }), state);
});

test('a lost stream stops the queue with a notice', () => {
  const next = scanQueueReducer(queued('a', 'b'), { type: 'streamLost', bucket: 'a' });
  assert.deepEqual(next.queue, []);
  assert.match(next.notice ?? '', /a/);
});

test('a failed start moves on with a notice', () => {
  const next = scanQueueReducer(queued('a', 'b'), { type: 'startFailed', bucket: 'a', error: '403' });
  assert.deepEqual(next.queue, ['b']);
  assert.match(next.notice ?? '', /403/);
});

test('replace, append, seed, stop and stopOne', () => {
  let s = scanQueueReducer(initialScanQueue, { type: 'seed', buckets: ['a'] });
  assert.deepEqual(s.queue, ['a']);
  s = scanQueueReducer(s, { type: 'seed', buckets: ['x'] });
  assert.deepEqual(s.queue, ['a'], 'seed fills an empty queue only');
  s = scanQueueReducer(s, { type: 'append', bucket: 'b' });
  s = scanQueueReducer(s, { type: 'append', bucket: 'b' });
  assert.deepEqual(s.queue, ['a', 'b']);
  s = scanQueueReducer(s, { type: 'stopOne', bucket: 'a' });
  assert.deepEqual(s.queue, ['b']);
  s = scanQueueReducer({ ...s, notice: 'old' }, { type: 'replace', buckets: ['c', 'd'] });
  assert.deepEqual(s, queued('c', 'd'));
  assert.deepEqual(scanQueueReducer(s, { type: 'stop' }), queued());
});
