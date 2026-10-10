/**
 * Bulk delete in batches, and the words the browser shows while it runs.
 *
 * React-free and fetch-free (the batch request is injected) so the node
 * unit project tests it — see src/__tests__/bulkDelete.test.ts. The hook
 * (useS3Browser.bulkDelete) owns the state; BulkActionBar shows it.
 */
import type { ListingProgress } from './bulkSelection';
import { normalizeUiError } from './errorHandling';
import { noun } from './utils';

/**
 * Keys per delete request. The server takes up to 10,000, but one request
 * that size shows no progress, cannot be cancelled, and a closed tab stops
 * it partway. Small batches give frequent progress and a short wait after
 * Cancel; the run has no total limit.
 */
export const DELETE_BATCH_SIZE = 500;

/** Failures kept for the report. Each answer carries up to 100; a long run would pile them up. */
export const MAX_REPORTED_FAILURES = 20;

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

/** What the action bar shows while a bulk delete runs. */
export type BulkDeleteProgress =
  | ({ phase: 'listing'; stopping: boolean } & ListingProgress)
  | { phase: 'deleting'; stopping: boolean; done: number; total: number };

/**
 * Send `keys` to `deleteBatch` in batches of `batchSize`, one batch at a
 * time, and add up the answers. An abort stops the run BEFORE the next batch
 * (the batch in flight finishes) and resolves with `cancelled: true`. A
 * failed batch rejects with its own error; `onBatch` already reported the
 * batches before it.
 */
export async function deleteInBatches(
  keys: string[],
  deleteBatch: (batch: string[]) => Promise<DeleteBatchResult>,
  {
    batchSize = DELETE_BATCH_SIZE,
    signal,
    onBatch,
  }: { batchSize?: number; signal?: AbortSignal; onBatch?: (progress: BatchProgress) => void } = {},
): Promise<BulkDeleteOutcome> {
  const total = keys.length;
  const outcome: BulkDeleteOutcome = { total, deleted: 0, failed: 0, failures: [], cancelled: false };
  for (let done = 0; done < total; ) {
    if (signal?.aborted) return { ...outcome, cancelled: true };
    const batch = keys.slice(done, done + batchSize);
    const answer = await deleteBatch(batch);
    done += batch.length;
    outcome.deleted += answer.deleted;
    outcome.failed += answer.failed;
    outcome.failures.push(...answer.failures.slice(0, MAX_REPORTED_FAILURES - outcome.failures.length));
    onBatch?.({ done, total, deleted: outcome.deleted, failed: outcome.failed });
  }
  return outcome;
}

const count = (n: number) => n.toLocaleString('en-US');

/** The status line of the action bar while a bulk delete runs. */
export function bulkDeleteProgressText(p: BulkDeleteProgress): string {
  if (p.phase === 'listing') {
    if (p.stopping) return 'Stopping…';
    if (p.folders === 0) return 'Preparing…';
    const found = p.keysFound > 0 ? ` ${count(p.keysFound)} ${noun(p.keysFound, 'object')} found` : '';
    return `Listing folders ${p.listed} of ${p.folders}…${found}`;
  }
  if (p.stopping) return `Stopping after this batch… ${count(p.done)} of ${count(p.total)} done`;
  return `Deleting ${count(p.done)} of ${count(p.total)}…`;
}

/** The progress bar's percent for the current phase. */
export function bulkDeleteProgressPercent(p: BulkDeleteProgress): number {
  const [part, whole] = p.phase === 'listing' ? [p.listed, p.folders] : [p.done, p.total];
  return whole === 0 ? 100 : Math.floor((part / whole) * 100);
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

/**
 * A bulk delete that stopped on an error. The message is the cause, then how
 * many keys the finished batches deleted before it; `total` 0 = the run
 * failed before its first delete request (while it listed the folders).
 */
export class BulkDeleteFailed extends Error {
  readonly cause: unknown;

  constructor(cause: unknown, deleted: number, total: number) {
    const tail =
      total === 0
        ? 'Nothing was deleted.'
        : `${count(deleted)} of ${count(total)} ${noun(total, 'object')} were deleted before the failure.`;
    super(`${normalizeUiError(cause, 'Bulk delete failed').replace(/[.\s]+$/, '')}. ${tail}`);
    this.name = 'BulkDeleteFailed';
    this.cause = cause;
  }
}
