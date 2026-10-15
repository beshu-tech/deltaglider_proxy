/**
 * Bulk delete in batches, and the words the browser shows after one.
 *
 * React-free and fetch-free (the batch request is injected) so the node
 * unit project tests it — see src/__tests__/bulkDelete.test.ts. The batch
 * loop and the progress words are shared with copy and move
 * (bulkBatches.ts). The hook (useS3Browser.bulkDelete) owns the run;
 * BulkActionBar shows it.
 */
import type { BulkDeleteProgress } from './adminApi/bulkObjects';
import { count, keepFailures, sendInBatches } from './bulkBatches';
import { noun } from './utils';

/** What a delete is about to remove, counted before the confirmation. */
export interface DeletePlanSummary {
  /** Objects to delete: the selected objects plus everything inside the selected folders. */
  objects: number;
  /** Selected folders. */
  folders: number;
  /** Selected objects outside the folders. */
  files: number;
}

/** From this many objects the confirmation explains how a long delete runs. */
const LARGE_DELETE_OBJECTS = 10_000;

/**
 * The words of the delete confirmation, shown after the selection is
 * counted: the exact number of objects, what the selection holds, and for a
 * large delete how it runs and how it stops.
 */
export function bulkDeleteConfirm(s: DeletePlanSummary): { title: string; body: string[]; okText: string } {
  const objects = `${count(s.objects)} ${noun(s.objects, 'object')}`;
  const parts = [
    s.folders > 0 ? `${count(s.folders)} ${noun(s.folders, 'folder')}` : '',
    s.files > 0 ? `${count(s.files)} ${noun(s.files, 'object')}` : '',
  ].filter(Boolean);
  const body =
    s.folders > 0
      ? [`You selected ${parts.join(' and ')}. With everything inside the ${noun(s.folders, 'folder')}, that is ${objects}. This cannot be undone.`]
      : [`This cannot be undone.`];
  if (s.objects >= LARGE_DELETE_OBJECTS) {
    body.push(
      `A delete this large runs from this browser tab and can take several minutes. The bar shows how far it got and the time left. Keep the tab open until it ends.`,
      `Cancel stops it after the batch in flight. Objects deleted before that stay deleted.`,
    );
  }
  return {
    title: `Delete ${objects} permanently?`,
    body,
    okText: s.objects >= LARGE_DELETE_OBJECTS ? `Delete ${objects}` : 'Delete',
  };
}

/** The answer of `POST /api/admin/objects/delete`. */
export interface DeleteBatchResult {
  deleted: number;
  failed: number;
  failures: { key: string; error: string }[];
}

export interface BulkDeleteOutcome extends DeleteBatchResult {
  /** Keys the run set out to delete. */
  total: number;
  /** The user stopped the run; the keys after the last finished batch are still there. */
  cancelled: boolean;
}

interface BatchProgress {
  /** Keys sent so far, deleted or failed. */
  done: number;
  total: number;
  deleted: number;
  failed: number;
}

/**
 * Send `keys` to `deleteBatch` in batches (see `sendInBatches`) and add up
 * the answers. The run has no total limit.
 *
 * `deleteBatch` may report the keys of its batch as they end (the
 * server's streamed totals, counted from the start of the batch);
 * `onProgress` then gets the totals of the whole run after each key, and
 * `onBatch` after each answer.
 */
export async function deleteInBatches(
  keys: string[],
  deleteBatch: (batch: string[], onKey: (progress: BulkDeleteProgress) => void) => Promise<DeleteBatchResult>,
  {
    batchSize,
    signal,
    onBatch,
    onProgress,
  }: {
    batchSize?: number;
    signal?: AbortSignal;
    onBatch?: (progress: BatchProgress) => void;
    onProgress?: (progress: BatchProgress) => void;
  } = {},
): Promise<BulkDeleteOutcome> {
  const total = keys.length;
  const outcome: BulkDeleteOutcome = { total, deleted: 0, failed: 0, failures: [], cancelled: false };
  let sent = 0;
  const { cancelled } = await sendInBatches(
    keys,
    (batch) => {
      // The batches before this one, which the server's totals start after.
      const before = { done: sent, deleted: outcome.deleted, failed: outcome.failed };
      const within = (n: number) => Math.min(Math.max(0, n), batch.length);
      return deleteBatch(batch, (p) =>
        onProgress?.({
          done: before.done + within(p.done),
          total,
          deleted: before.deleted + within(p.deleted),
          failed: before.failed + within(p.failed),
        }),
      );
    },
    {
      batchSize,
      signal,
      onAnswer: (answer, done) => {
        sent = done;
        outcome.deleted += answer.deleted;
        outcome.failed += answer.failed;
        keepFailures(outcome.failures, answer.failures);
        onBatch?.({ done, total, deleted: outcome.deleted, failed: outcome.failed });
      },
    },
  );
  return { ...outcome, cancelled };
}

/** The message after a run that did not throw. */
export function bulkDeleteOutcomeMessage(o: BulkDeleteOutcome): { type: 'success' | 'info' | 'warning'; text: string } {
  const objects = `${count(o.total)} ${noun(o.total, 'object')}`;
  if (o.cancelled) {
    if (o.deleted === 0 && o.failed === 0) return { type: 'info', text: 'Delete cancelled. Nothing was deleted.' };
    return { type: 'info', text: `Delete stopped. ${count(o.deleted)} of ${objects} deleted.` };
  }
  if (o.failed > 0) {
    const first = o.failures[0];
    const example = first ? `, for example ${first.key}: ${first.error}` : '';
    return { type: 'warning', text: `${count(o.deleted)} of ${objects} deleted. ${count(o.failed)} failed${example}` };
  }
  if (o.total === 0) return { type: 'info', text: 'Nothing to delete.' };
  return { type: 'success', text: `${count(o.deleted)} ${noun(o.deleted, 'object')} deleted` };
}
