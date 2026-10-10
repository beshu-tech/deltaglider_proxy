/**
 * The batch loop and the progress words shared by the browser's bulk
 * actions (delete, copy, move).
 *
 * React-free and fetch-free (the batch request is injected) so the node
 * unit project tests it — see src/__tests__/bulkBatches.test.ts. The hook
 * (useS3Browser) owns the run; BulkActionBar shows its progress.
 */
import type { ListingProgress } from './bulkSelection';
import { normalizeUiError } from './errorHandling';
import { noun } from './utils';

export type BulkAction = 'delete' | 'copy' | 'move';

/**
 * Items per request. The server takes up to 10,000, but one request that
 * size shows no progress, cannot be cancelled, and a closed tab stops it
 * partway. Small batches give frequent progress and a short wait after
 * Cancel.
 */
export const BULK_BATCH_SIZE = 500;

/** Failures kept for the report. Each answer carries up to 100; a long run would pile them up. */
export const MAX_REPORTED_FAILURES = 20;

/** What the action bar shows while a bulk action runs. */
export type BulkProgress =
  | ({ action: BulkAction; phase: 'listing'; stopping: boolean } & ListingProgress)
  | { action: BulkAction; phase: 'sending'; stopping: boolean; done: number; total: number };

/**
 * Send `items` to `sendBatch` in batches of `batchSize`, one batch at a
 * time. `onAnswer` gets each answer and how many items went so far. An
 * abort stops the run BEFORE the next batch (the batch in flight finishes)
 * and resolves with `cancelled: true`. A failed batch rejects with its own
 * error; `onAnswer` already saw the batches before it.
 */
export async function sendInBatches<T, A>(
  items: T[],
  sendBatch: (batch: T[]) => Promise<A>,
  {
    batchSize = BULK_BATCH_SIZE,
    signal,
    onAnswer,
  }: { batchSize?: number; signal?: AbortSignal; onAnswer?: (answer: A, done: number) => void } = {},
): Promise<{ cancelled: boolean }> {
  for (let done = 0; done < items.length; ) {
    if (signal?.aborted) return { cancelled: true };
    const batch = items.slice(done, done + batchSize);
    const answer = await sendBatch(batch);
    done += batch.length;
    onAnswer?.(answer, done);
  }
  return { cancelled: false };
}

/** Append `more` to `kept` up to MAX_REPORTED_FAILURES entries. */
export function keepFailures<F>(kept: F[], more: F[]): void {
  kept.push(...more.slice(0, Math.max(0, MAX_REPORTED_FAILURES - kept.length)));
}

export const count = (n: number) => n.toLocaleString('en-US');

const RUNNING: Record<BulkAction, string> = { delete: 'Deleting', copy: 'Copying', move: 'Moving' };
const DONE: Record<BulkAction, string> = { delete: 'deleted', copy: 'copied', move: 'moved' };
/** What the reports count: a delete says objects, copy and move say items (as their picker does). */
export const UNIT: Record<BulkAction, string> = { delete: 'object', copy: 'item', move: 'item' };

/** The status line of the action bar while a bulk action runs. */
export function bulkProgressText(p: BulkProgress): string {
  if (p.phase === 'listing') {
    if (p.stopping) return 'Stopping…';
    if (p.folders === 0) return 'Preparing…';
    const found = p.keysFound > 0 ? ` ${count(p.keysFound)} ${noun(p.keysFound, 'object')} found` : '';
    return `Listing folders ${p.listed} of ${p.folders}…${found}`;
  }
  if (p.stopping) return `Stopping after this batch… ${count(p.done)} of ${count(p.total)} done`;
  return `${RUNNING[p.action]} ${count(p.done)} of ${count(p.total)}…`;
}

/** The progress bar's percent for the current phase. */
export function bulkProgressPercent(p: BulkProgress): number {
  const [part, whole] = p.phase === 'listing' ? [p.listed, p.folders] : [p.done, p.total];
  return whole === 0 ? 100 : Math.floor((part / whole) * 100);
}

/**
 * A bulk action that stopped on an error. The message is the cause, then
 * how many items the finished batches handled before it; `total` 0 = the
 * run failed before its first request (while it listed the folders).
 */
export class BulkActionFailed extends Error {
  readonly cause: unknown;

  constructor(action: BulkAction, cause: unknown, done: number, total: number) {
    const tail =
      total === 0
        ? `Nothing was ${DONE[action]}.`
        : `${count(done)} of ${count(total)} ${noun(total, UNIT[action])} were ${DONE[action]} before the failure.`;
    super(`${normalizeUiError(cause, `Bulk ${action} failed`).replace(/[.\s]+$/, '')}. ${tail}`);
    this.name = 'BulkActionFailed';
    this.cause = cause;
  }
}
