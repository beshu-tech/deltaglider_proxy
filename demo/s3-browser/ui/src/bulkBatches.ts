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
 * Items per request. The server takes up to 10,000, but Cancel waits for
 * the request in flight, which the server finishes in any case. Small
 * batches give a short wait after Cancel, and copy and move show progress
 * only per batch (a delete also streams progress per key).
 */
export const BULK_BATCH_SIZE = 500;

/** Failures kept for the report. Each answer carries up to 100; a long run would pile them up. */
export const MAX_REPORTED_FAILURES = 20;

/** What the action bar shows while a bulk action runs. */
export type BulkProgress =
  | ({ action: BulkAction; phase: 'listing'; stopping: boolean } & ListingProgress)
  /** Counted; the confirmation dialog is open and nothing has started. */
  | { action: BulkAction; phase: 'confirming'; stopping: boolean; total: number }
  | {
      action: BulkAction;
      phase: 'sending';
      stopping: boolean;
      done: number;
      total: number;
      /** `Date.now()` when the first batch went: the time left is estimated from it. */
      startedAt?: number;
    };

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

/** No time left is shown before the run has gone this long: the first batches say little about the rate. */
const ESTIMATE_AFTER_MS = 5_000;

/** "about 4 min left", from the time per item so far; '' while too early to tell. */
export function timeLeftText(done: number, total: number, elapsedMs: number): string {
  if (done <= 0 || done >= total || elapsedMs < ESTIMATE_AFTER_MS) return '';
  const leftMs = (elapsedMs / done) * (total - done);
  if (leftMs < 60_000) return 'less than a minute left';
  const minutes = Math.round(leftMs / 60_000);
  if (minutes < 90) return `about ${minutes} min left`;
  return `about ${Math.round(minutes / 60)} h left`;
}

/**
 * The status line of the action bar while a bulk action runs. A delete
 * counts its objects first and says that nothing is deleted yet.
 */
export function bulkProgressText(p: BulkProgress, now: number = Date.now()): string {
  if (p.phase === 'confirming') {
    return `${count(p.total)} ${noun(p.total, UNIT[p.action])} to ${p.action}. Waiting for your confirmation.`;
  }
  if (p.phase === 'listing') {
    if (p.stopping) return 'Stopping…';
    if (p.action === 'delete') {
      const where = p.folders > 1 ? ` in folder ${Math.min(p.listed + 1, p.folders)} of ${p.folders}` : '';
      return `Counting objects to delete${where}… ${count(p.keysFound)} found. Nothing is deleted yet.`;
    }
    if (p.folders === 0) return 'Preparing…';
    const found = p.keysFound > 0 ? ` ${count(p.keysFound)} ${noun(p.keysFound, 'object')} found` : '';
    return `Listing folders ${p.listed} of ${p.folders}…${found}`;
  }
  if (p.stopping) return `Stopping after this batch… ${count(p.done)} of ${count(p.total)} done`;
  const percent = p.total > 0 ? ` · ${bulkProgressPercent(p)}%` : '';
  const left = p.startedAt === undefined ? '' : timeLeftText(p.done, p.total, now - p.startedAt);
  return `${RUNNING[p.action]} ${count(p.done)} of ${count(p.total)}${percent}${left ? ` · ${left}` : '…'}`;
}

/**
 * The progress bar's percent for the current phase; `null` while a delete
 * counts (the size of a folder is unknown until it is listed).
 */
export function bulkProgressPercent(p: BulkProgress): number | null {
  if (p.phase === 'confirming' || (p.phase === 'listing' && p.action === 'delete')) return null;
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
