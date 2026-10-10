import assert from 'node:assert/strict';
import { test } from 'vitest';
import {
  BulkActionFailed,
  bulkProgressPercent,
  bulkProgressText,
  sendInBatches,
  type BulkProgress,
} from '../bulkBatches';

// The loop and the words shared by bulk delete, copy and move. The
// delete-specific sums are in bulkDelete.test.ts, copy/move in bulkTransfer.test.ts.

test('the loop sends fixed-size batches one at a time and stops before the next batch on abort', async () => {
  const ctl = new AbortController();
  const sent: number[] = [];
  const answers: [string, number][] = [];
  const outcome = await sendInBatches(
    Array.from({ length: 12 }, (_, i) => i),
    async (batch) => {
      sent.push(batch.length);
      if (sent.length === 2) ctl.abort();
      return `answer ${sent.length}`;
    },
    { batchSize: 5, signal: ctl.signal, onAnswer: (a, done) => answers.push([a, done]) },
  );
  assert.deepEqual(sent, [5, 5]);
  assert.deepEqual(answers, [['answer 1', 5], ['answer 2', 10]]);
  assert.deepEqual(outcome, { cancelled: true });
  assert.deepEqual(await sendInBatches([], async () => 'never'), { cancelled: false });
});

test('progress text and percent, per phase and action', () => {
  const listing: BulkProgress = { action: 'delete', phase: 'listing', listed: 12, folders: 50, keysFound: 4310, stopping: false };
  assert.equal(bulkProgressText(listing), 'Listing folders 12 of 50… 4,310 objects found');
  assert.equal(bulkProgressPercent(listing), 24);
  assert.equal(bulkProgressText({ ...listing, listed: 0, keysFound: 0 }), 'Listing folders 0 of 50…');
  assert.equal(bulkProgressText({ ...listing, folders: 0, listed: 0, keysFound: 0 }), 'Preparing…');
  assert.equal(bulkProgressText({ ...listing, stopping: true }), 'Stopping…');

  const sending: BulkProgress = { action: 'delete', phase: 'sending', done: 3400, total: 9800, stopping: false };
  assert.equal(bulkProgressText(sending), 'Deleting 3,400 of 9,800…');
  assert.equal(bulkProgressText({ ...sending, action: 'copy' }), 'Copying 3,400 of 9,800…');
  assert.equal(bulkProgressText({ ...sending, action: 'move' }), 'Moving 3,400 of 9,800…');
  assert.equal(bulkProgressPercent(sending), 34, 'rounds down: 100 only when done');
  assert.equal(bulkProgressText({ ...sending, stopping: true }), 'Stopping after this batch… 3,400 of 9,800 done');
  assert.equal(bulkProgressPercent({ ...sending, done: 0, total: 0 }), 100);
});

test('a failure names how many objects went before it, and keeps its cause', () => {
  const disk = new Error('Bulk delete failed (500): disk full');
  const failed = new BulkActionFailed('delete', disk, 3400, 9800);
  assert.equal(failed.message, 'Bulk delete failed (500): disk full. 3,400 of 9,800 objects were deleted before the failure.');
  assert.equal(failed.cause, disk);
  // The first batch failed: none confirmed, but the run had started.
  assert.equal(
    new BulkActionFailed('delete', new Error('Bulk delete failed (500): denied'), 0, 3).message,
    'Bulk delete failed (500): denied. 0 of 3 objects were deleted before the failure.',
  );
  assert.equal(
    new BulkActionFailed('copy', new Error('Bulk copy failed (502): bad gateway'), 500, 1201).message,
    'Bulk copy failed (502): bad gateway. 500 of 1,201 items were copied before the failure.',
  );
  assert.equal(
    new BulkActionFailed('move', new Error('Bulk move failed (502): bad gateway'), 1, 2).message,
    'Bulk move failed (502): bad gateway. 1 of 2 items were moved before the failure.',
  );
  // The listing failed: no request was sent.
  assert.equal(
    new BulkActionFailed('delete', new Error('Folder big/ has more than 10,000 objects; narrow the selection.'), 0, 0).message,
    'Folder big/ has more than 10,000 objects; narrow the selection. Nothing was deleted.',
  );
  assert.match(new BulkActionFailed('move', new Error('x'), 0, 0).message, /Nothing was moved\.$/);
});
