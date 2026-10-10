import assert from 'node:assert/strict';
import { test } from 'vitest';
import { BULK_BATCH_SIZE, MAX_REPORTED_FAILURES } from '../bulkBatches';
import {
  bulkTransferOutcomeMessage,
  transferInBatches,
  transferPlanError,
  type BulkTransferOutcome,
  type TransferBatchResult,
} from '../bulkTransfer';

// Bulk copy and move went to the server as ONE request of the whole
// selection, run one item at a time: no progress, no cancel. They now go in
// batches like the bulk delete.

const items = (n: number) => Array.from({ length: n }, (_, i) => ({ source: `src/${i}.zip`, relative: `${i}.zip` }));
const ok = (batch: unknown[], deleted = batch.length): TransferBatchResult => ({
  succeeded: batch.length,
  failed: 0,
  deleted,
  failures: [],
});

test(`items go in batches of ${BULK_BATCH_SIZE}, one request at a time, with progress after each`, async () => {
  const sent: number[] = [];
  let inFlight = 0;
  let maxInFlight = 0;
  const progress: unknown[] = [];
  const outcome = await transferInBatches(
    'copy',
    items(1201),
    async (batch) => {
      sent.push(batch.length);
      maxInFlight = Math.max(maxInFlight, ++inFlight);
      await new Promise((r) => setTimeout(r, 1));
      inFlight--;
      return { succeeded: batch.length, failed: 0, failures: [] };
    },
    { onBatch: (p) => progress.push(p) },
  );
  assert.deepEqual(sent, [500, 500, 201]);
  assert.equal(maxInFlight, 1, 'batches are sequential');
  assert.deepEqual(progress, [
    { done: 500, went: 500 },
    { done: 1000, went: 1000 },
    { done: 1201, went: 1201 },
  ]);
  assert.deepEqual(outcome, {
    action: 'copy', total: 1201, succeeded: 1201, failed: 0, deleted: 0, failures: [], cancelled: false,
  });
});

test('a move counts the removed sources: a batch with a failed copy keeps all of its own', async () => {
  let n = 0;
  const outcome = await transferInBatches('move', items(1500), async (batch) => {
    if (++n !== 2) return ok(batch);
    return {
      succeeded: batch.length - 1,
      failed: 1,
      deleted: 0,
      failures: [{ source_key: batch[0].source, dest_key: `old/${batch[0].relative}`, error: 'quota exceeded.' }],
    };
  });
  assert.equal(outcome.succeeded, 1499);
  assert.equal(outcome.failed, 1);
  assert.equal(outcome.deleted, 1000);
  assert.deepEqual(bulkTransferOutcomeMessage(outcome), {
    type: 'warning',
    text: '1,000 of 1,500 items moved. 1 failed, for example src/500.zip: quota exceeded. 499 copied items also stay in the source folder. A batch with a failed copy keeps all its sources.',
  });
});

test('failures add up across batches; the kept list is capped', async () => {
  const outcome = await transferInBatches('copy', items(1500), async (batch) => ({
    succeeded: batch.length - 10,
    failed: 10,
    failures: batch.slice(0, 10).map((i) => ({ source_key: i.source, dest_key: i.relative, error: 'AccessDenied' })),
  }));
  assert.equal(outcome.failed, 30);
  assert.equal(outcome.failures.length, MAX_REPORTED_FAILURES);
});

test('cancel: the batch in flight finishes, then nothing more is sent', async () => {
  const ctl = new AbortController();
  const sent: number[] = [];
  const outcome = await transferInBatches(
    'move',
    items(1201),
    async (batch) => {
      sent.push(batch.length);
      ctl.abort();
      return ok(batch);
    },
    { signal: ctl.signal },
  );
  assert.deepEqual(sent, [500]);
  assert.deepEqual(outcome, {
    action: 'move', total: 1201, succeeded: 500, failed: 0, deleted: 500, failures: [], cancelled: true,
  });
});

test('the whole plan is checked before the first batch: the server sees one batch at a time', () => {
  const plan = [
    { source: 'builds/a/x.zip', relative: 'a/x.zip' },
    { source: 'builds/b/x.zip', relative: 'b/x.zip' },
  ];
  assert.equal(transferPlanError(plan, 'releases', 'releases', 'old/'), null);
  assert.equal(transferPlanError(plan, 'releases', 'archive', ''), null);
  assert.equal(
    transferPlanError([...plan, { source: 'other/x.zip', relative: 'a/x.zip' }], 'releases', 'archive', 'old/'),
    '1 destination key would receive two objects, for example old/a/x.zip; narrow the selection.',
  );
  // builds/a/ into builds/: each item lands on its own source (a no-op the server skips)…
  assert.equal(transferPlanError(plan, 'releases', 'releases', 'builds/'), null);
  // …but builds/a/ into itself puts builds/a/x.zip on builds/a/a/x.zip, another selected object.
  const nested = [{ source: 'builds/a/a/x.zip', relative: 'a/a/x.zip' }, { source: 'builds/a/x.zip', relative: 'a/x.zip' }];
  assert.equal(
    transferPlanError(nested, 'releases', 'releases', 'builds/a/'),
    'The destination builds/a/a/x.zip is also a selected source, so it would be overwritten before it is copied. Is the destination inside the selection?',
  );
  // Another bucket: no source is overwritten.
  assert.equal(transferPlanError(nested, 'releases', 'archive', 'builds/a/'), null);
});

test('the report after a run', () => {
  const copy: BulkTransferOutcome = {
    action: 'copy', total: 1201, succeeded: 1201, failed: 0, deleted: 0, failures: [], cancelled: false,
  };
  const move: BulkTransferOutcome = { ...copy, action: 'move', deleted: 1201 };
  assert.deepEqual(bulkTransferOutcomeMessage(copy), { type: 'success', text: '1,201 items copied' });
  assert.deepEqual(bulkTransferOutcomeMessage(move), { type: 'success', text: '1,201 items moved' });
  assert.deepEqual(bulkTransferOutcomeMessage({ ...copy, total: 1, succeeded: 1 }), { type: 'success', text: '1 item copied' });
  assert.deepEqual(bulkTransferOutcomeMessage({ ...copy, total: 0, succeeded: 0 }), { type: 'info', text: 'Nothing to copy.' });
  assert.deepEqual(bulkTransferOutcomeMessage({ ...move, succeeded: 0, deleted: 0, cancelled: true }), {
    type: 'info',
    text: 'Move cancelled. Nothing was moved.',
  });
  assert.deepEqual(bulkTransferOutcomeMessage({ ...copy, succeeded: 500, cancelled: true }), {
    type: 'info',
    text: 'Copy stopped. 500 of 1,201 items copied.',
  });
  assert.deepEqual(
    bulkTransferOutcomeMessage({
      ...copy,
      succeeded: 1200,
      failed: 1,
      failures: [{ source_key: 'a/b.zip', dest_key: 'old/b.zip', error: 'AccessDenied' }],
    }),
    { type: 'warning', text: '1,200 of 1,201 items copied. 1 failed, for example a/b.zip: AccessDenied.' },
  );
  // A move whose sources were not all removed (one changed during the copy) says so.
  assert.deepEqual(bulkTransferOutcomeMessage({ ...move, deleted: 1200 }), {
    type: 'warning',
    text: '1,200 of 1,201 items moved. 1 copied item also stays in the source folder.',
  });
});
