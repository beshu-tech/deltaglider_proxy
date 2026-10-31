/**
 * The batch loop and the progress words shared by the browser's bulk
 * actions (delete, copy, move).
 *
 * React-free and fetch-free (the batch request is injected) so the node
 * unit project tests it — see src/__tests__/bulkBatches.test.ts. The hook
 * (useS3Browser) owns the run; BulkActionBar shows its progress.
 */
import type { ListingProgress } from './bulkSelection';
import { plainError } from './plainError';
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
  /** Counted; the confirmation dialog is open and nothing has started. The
   *  dialog is the one surface then: the action bar shows nothing. */
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
/** What the reports count: objects, for every action (folders are expanded to their objects). */
export const UNIT: Record<BulkAction, string> = { delete: 'object', copy: 'object', move: 'object' };

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

/** A bulk action that the bar shows: every phase but the confirmation, which the dialog owns. */
export type ShownBulkProgress = Exclude<BulkProgress, { phase: 'confirming' }>;

/**
 * The status line of the action bar while a bulk action runs. A delete
 * counts its objects first and says that nothing is deleted yet.
 */
export function bulkProgressText(p: ShownBulkProgress, now: number = Date.now()): string {
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
  // A delete only starts its server job here.
  if (p.action === 'delete') return `Starting the delete of ${count(p.total)} ${noun(p.total, 'object')}…`;
  if (p.stopping) return `Stopping after this batch… ${count(p.done)} of ${count(p.total)} done`;
  const percent = p.total > 0 ? ` · ${bulkProgressPercent(p)}%` : '';
  const left = p.startedAt === undefined ? '' : timeLeftText(p.done, p.total, now - p.startedAt);
  return `${RUNNING[p.action]} ${count(p.done)} of ${count(p.total)}${percent}${left ? ` · ${left}` : '…'}`;
}

/**
 * The progress bar's percent for the current phase; `null` while a delete
 * counts (the size of a folder is unknown until it is listed).
 */
export function bulkProgressPercent(p: ShownBulkProgress): number | null {
  // A delete only counts and starts its job here: no percent to show.
  if (p.action === 'delete') return null;
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
  /** The raw text of the cause (status, code, request id), for a "Details" disclosure. */
  readonly details?: string;

  constructor(action: BulkAction, cause: unknown, done: number, total: number) {
    const tail =
      total === 0
        ? `Nothing was ${DONE[action]}.`
        : `${count(done)} of ${count(total)} ${noun(total, UNIT[action])} were ${DONE[action]} before the failure.`;
    // The plain sentence of the cause (plainError), never the wire text.
    const plain = plainError(cause, `The ${action} failed.`);
    super(`${plain.text.replace(/[.\s]+$/, '')}. ${tail}`);
    this.name = 'BulkActionFailed';
    this.cause = cause;
    this.details = plain.details;
  }
}
