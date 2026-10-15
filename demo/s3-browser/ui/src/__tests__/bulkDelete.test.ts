import assert from 'node:assert/strict';
import { test } from 'vitest';
import { BULK_BATCH_SIZE, MAX_REPORTED_FAILURES } from '../bulkBatches';
import { bulkDeleteConfirm, bulkDeleteOutcomeMessage, deleteInBatches, type DeleteBatchResult } from '../bulkDelete';

// A page of ~50 folders went to the server as ONE delete request: no
// progress, no cancel, and more than 10,000 keys failed at the very end.
// deleteInBatches sends fixed-size batches one after the other.

const keys = (n: number) => Array.from({ length: n }, (_, i) => `k${i}`);
const allDeleted = async (batch: string[]): Promise<DeleteBatchResult> => ({ deleted: batch.length, failed: 0, failures: [] });

test(`keys go in batches of ${BULK_BATCH_SIZE}, one request at a time, with progress after each`, async () => {
  assert.equal(BULK_BATCH_SIZE, 500);
  const sent: string[][] = [];
  let inFlight = 0;
  let maxInFlight = 0;
  const progress: unknown[] = [];
  const outcome = await deleteInBatches(
    keys(1201),
    async (batch) => {
      sent.push(batch);
      maxInFlight = Math.max(maxInFlight, ++inFlight);
      await new Promise((r) => setTimeout(r, 1));
      inFlight--;
      return allDeleted(batch);
    },
    { onBatch: (p) => progress.push({ ...p }) },
  );
  assert.deepEqual(sent.map((b) => b.length), [500, 500, 201]);
  assert.deepEqual(sent.flat(), keys(1201), 'every key once, in order');
  assert.equal(maxInFlight, 1, 'batches are sequential');
  assert.deepEqual(progress, [
    { done: 500, total: 1201, deleted: 500, failed: 0 },
    { done: 1000, total: 1201, deleted: 1000, failed: 0 },
    { done: 1201, total: 1201, deleted: 1201, failed: 0 },
  ]);
  assert.deepEqual(outcome, { total: 1201, deleted: 1201, failed: 0, failures: [], cancelled: false });
});

// The bar moved only when a whole batch answered: a folder of 2 files showed
// "Deleting 0 of 2…" until the end. The server reports each key of a batch
// as it ends; the run adds the batches before it.
test('progress moves with each key inside a batch, added to the batches before it', async () => {
  const progress: unknown[] = [];
  const batches: unknown[] = [];
  const outcome = await deleteInBatches(
    keys(3),
    async (batch, onKey) => {
      // The second key of the first batch fails.
      batch.forEach((_, i) => onKey({ done: i + 1, deleted: i + 1 - Number(batch[i] === 'k1'), failed: Number(batch[i] === 'k1') }));
      const failed = batch.includes('k1') ? 1 : 0;
      return { deleted: batch.length - failed, failed, failures: failed ? [{ key: 'k1', error: 'AccessDenied' }] : [] };
    },
    { batchSize: 2, onProgress: (p) => progress.push({ ...p }), onBatch: (p) => batches.push({ ...p }) },
  );
  assert.deepEqual(progress, [
    { done: 1, total: 3, deleted: 1, failed: 0 },
    { done: 2, total: 3, deleted: 1, failed: 1 },
    { done: 3, total: 3, deleted: 2, failed: 1 },
  ]);
  assert.deepEqual(batches, [
    { done: 2, total: 3, deleted: 1, failed: 1 },
    { done: 3, total: 3, deleted: 2, failed: 1 },
  ]);
  assert.equal(outcome.deleted, 2);
});

test('a key report never counts past its own batch', async () => {
  const progress: number[] = [];
  await deleteInBatches(
    keys(3),
    async (batch, onKey) => {
      onKey({ done: 99, deleted: 99, failed: 0 });
      return allDeleted(batch);
    },
    { batchSize: 2, onProgress: (p) => progress.push(p.done) },
  );
  assert.deepEqual(progress, [2, 3]);
});

test('more than 10,000 keys is no longer one oversized request', async () => {
  const sizes: number[] = [];
  await deleteInBatches(keys(12_345), async (b) => {
    sizes.push(b.length);
    return allDeleted(b);
  });
  assert.equal(Math.max(...sizes), BULK_BATCH_SIZE);
  assert.equal(sizes.reduce((a, b) => a + b, 0), 12_345);
});

test('failures add up across batches; the kept list is capped', async () => {
  const outcome = await deleteInBatches(keys(1500), async (batch) => ({
    deleted: batch.length - 10,
    failed: 10,
    failures: batch.slice(0, 10).map((key) => ({ key, error: 'AccessDenied' })),
  }));
  assert.equal(outcome.deleted, 1470);
  assert.equal(outcome.failed, 30);
  assert.equal(outcome.failures.length, MAX_REPORTED_FAILURES);
  assert.deepEqual(outcome.failures[0], { key: 'k0', error: 'AccessDenied' });
});

test('cancel: the batch in flight finishes, then nothing more is sent', async () => {
  const ctl = new AbortController();
  const sent: number[] = [];
  const outcome = await deleteInBatches(
    keys(1201),
    async (batch) => {
      sent.push(batch.length);
      ctl.abort(); // the user cancels while this batch runs
      return allDeleted(batch);
    },
    { signal: ctl.signal },
  );
  assert.deepEqual(sent, [500]);
  assert.deepEqual(outcome, { total: 1201, deleted: 500, failed: 0, failures: [], cancelled: true });
});

test('a failed batch rejects with its own error; earlier batches were reported', async () => {
  const progress: number[] = [];
  const boom = new Error('backend down');
  let n = 0;
  await assert.rejects(
    deleteInBatches(
      keys(1201),
      async (batch) => {
        if (++n === 2) throw boom;
        return allDeleted(batch);
      },
      { onBatch: (p) => progress.push(p.deleted) },
    ),
    (e) => e === boom,
  );
  assert.deepEqual(progress, [500]);
});

test('the report after a run', () => {
  const base = { total: 9800, deleted: 9800, failed: 0, failures: [], cancelled: false };
  assert.deepEqual(bulkDeleteOutcomeMessage(base), { type: 'success', text: '9,800 objects deleted' });
  assert.deepEqual(bulkDeleteOutcomeMessage({ ...base, total: 1, deleted: 1 }), { type: 'success', text: '1 object deleted' });
  assert.deepEqual(bulkDeleteOutcomeMessage({ ...base, total: 0, deleted: 0 }), { type: 'info', text: 'Nothing to delete.' });
  assert.deepEqual(bulkDeleteOutcomeMessage({ ...base, total: 0, deleted: 0, cancelled: true }), {
    type: 'info',
    text: 'Delete cancelled. Nothing was deleted.',
  });
  assert.deepEqual(bulkDeleteOutcomeMessage({ ...base, deleted: 3400, cancelled: true }), {
    type: 'info',
    text: 'Delete stopped. 3,400 of 9,800 objects deleted.',
  });
  assert.deepEqual(
    bulkDeleteOutcomeMessage({ ...base, deleted: 9700, failed: 100, failures: [{ key: 'a/b.zip', error: 'AccessDenied' }] }),
    { type: 'warning', text: '9,700 of 9,800 objects deleted. 100 failed, for example a/b.zip: AccessDenied' },
  );
});

test('the delete confirmation gives the exact count, and explains a large delete', () => {
  assert.deepEqual(bulkDeleteConfirm({ objects: 2, folders: 0, files: 2 }), {
    title: 'Delete 2 objects permanently?',
    body: ['This cannot be undone.'],
    okText: 'Delete',
  });
  assert.deepEqual(bulkDeleteConfirm({ objects: 1_201, folders: 1, files: 1 }), {
    title: 'Delete 1,201 objects permanently?',
    body: ['You selected 1 folder and 1 object. With everything inside the folder, that is 1,201 objects. This cannot be undone.'],
    okText: 'Delete',
  });
  const large = bulkDeleteConfirm({ objects: 230_412, folders: 2, files: 0 });
  assert.equal(large.title, 'Delete 230,412 objects permanently?');
  assert.equal(large.body[0], 'You selected 2 folders. With everything inside the folders, that is 230,412 objects. This cannot be undone.');
  assert.match(large.body[1], /runs from this browser tab .* Keep the tab open until it ends\.$/);
  assert.equal(large.body[2], 'Cancel stops it after the batch in flight. Objects deleted before that stay deleted.');
  assert.equal(large.okText, 'Delete 230,412 objects', 'a large delete names the count on its button');
});
