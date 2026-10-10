/**
 * Bulk copy and move in batches, and the words the browser shows after one.
 *
 * React-free and fetch-free (the batch request is injected) so the node
 * unit project tests it — see src/__tests__/bulkTransfer.test.ts. The hook
 * (useS3Browser.bulkCopy / bulkMove) owns the run; BulkActionBar shows it.
 */
import { type BulkAction, UNIT, count, keepFailures, sendInBatches } from './bulkBatches';
import type { ExpandedItem } from './bulkSelection';
import { noun } from './utils';

type TransferAction = Extract<BulkAction, 'copy' | 'move'>;

/** The answer of `POST /api/admin/objects/copy` (`deleted` only from `/move`). */
export interface TransferBatchResult {
  succeeded: number;
  failed: number;
  /** Move: the sources removed. A batch with a failed copy removes none of its sources. */
  deleted?: number;
  failures: { source_key: string; dest_key: string; error: string }[];
}

export interface BulkTransferOutcome {
  action: TransferAction;
  /** Objects the run set out to copy or move. */
  total: number;
  /** Copies made. */
  succeeded: number;
  failed: number;
  /** Move: sources removed (an object moved). Copy: 0. */
  deleted: number;
  failures: TransferBatchResult['failures'];
  /** The user stopped the run; the items after the last finished batch were not sent. */
  cancelled: boolean;
}

interface TransferProgress {
  /** Items sent so far. */
  done: number;
  /** Objects copied (copy) or moved (move) so far. */
  went: number;
}

/**
 * Send `items` to `sendBatch` in batches (see `sendInBatches`) and add up the
 * answers. For a move, the server removes a batch's sources only when every
 * copy of that batch succeeded.
 */
export async function transferInBatches(
  action: TransferAction,
  items: ExpandedItem[],
  sendBatch: (batch: ExpandedItem[]) => Promise<TransferBatchResult>,
  { batchSize, signal, onBatch }: { batchSize?: number; signal?: AbortSignal; onBatch?: (progress: TransferProgress) => void } = {},
): Promise<BulkTransferOutcome> {
  const outcome: BulkTransferOutcome = {
    action,
    total: items.length,
    succeeded: 0,
    failed: 0,
    deleted: 0,
    failures: [],
    cancelled: false,
  };
  const { cancelled } = await sendInBatches(items, sendBatch, {
    batchSize,
    signal,
    onAnswer: (answer, done) => {
      outcome.succeeded += answer.succeeded;
      outcome.failed += answer.failed;
      if (action === 'move') outcome.deleted += answer.deleted ?? 0;
      keepFailures(outcome.failures, answer.failures);
      onBatch?.({ done, went: transferred(outcome) });
    },
  });
  return { ...outcome, cancelled };
}

/** Objects that got where they were sent: copies, or for a move the removed sources. */
function transferred(o: Pick<BulkTransferOutcome, 'action' | 'succeeded' | 'deleted'>): number {
  return o.action === 'copy' ? o.succeeded : o.deleted;
}

/**
 * Why the whole plan cannot run, or null. The server checks the same two
 * rules, but only inside one request; a batch cannot see the others:
 * - two items to one destination key (the second overwrites the first);
 * - in the same bucket, a destination that is another selected object
 *   (it is overwritten before it is copied, and a move then deletes it).
 * Two bucket names that alias one storage still reach only the server's
 * per-request check.
 */
export function transferPlanError(
  items: ExpandedItem[],
  sourceBucket: string,
  destBucket: string,
  destPrefix: string,
): string | null {
  const seen = new Set<string>();
  const doubled = new Set<string>();
  for (const it of items) {
    const dest = destPrefix + it.relative;
    if (seen.has(dest)) doubled.add(dest);
    seen.add(dest);
  }
  if (doubled.size > 0) {
    const [first] = doubled;
    return `${count(doubled.size)} destination ${noun(doubled.size, 'key')} would receive two objects, for example ${first}; narrow the selection.`;
  }
  if (sourceBucket !== destBucket) return null;
  const sources = new Set(items.map((i) => i.source));
  for (const it of items) {
    const dest = destPrefix + it.relative;
    if (dest !== it.source && sources.has(dest)) {
      return `The destination ${dest} is also a selected source, so it would be overwritten before it is copied. Is the destination inside the selection?`;
    }
  }
  return null;
}

/** The message after a copy or move that did not throw. */
export function bulkTransferOutcomeMessage(o: BulkTransferOutcome): { type: 'success' | 'info' | 'warning'; text: string } {
  const past = o.action === 'copy' ? 'copied' : 'moved';
  const Verb = o.action === 'copy' ? 'Copy' : 'Move';
  const went = transferred(o);
  const unit = UNIT[o.action];
  const items = `${count(o.total)} ${noun(o.total, unit)}`;
  // A move whose copy succeeded but whose source stayed: a batch with a
  // failed copy keeps all its sources.
  const kept = o.action === 'move' ? o.succeeded - o.deleted : 0;
  const keptText =
    kept > 0 ? ` ${count(kept)} copied ${noun(kept, unit)} also ${kept === 1 ? 'stays' : 'stay'} in the source folder.` : '';
  if (o.cancelled) {
    if (o.succeeded === 0 && o.failed === 0) return { type: 'info', text: `${Verb} cancelled. Nothing was ${past}.` };
    return { type: 'info', text: `${Verb} stopped. ${count(went)} of ${items} ${past}.${keptText}` };
  }
  if (o.failed > 0) {
    const first = o.failures[0];
    const example = first ? `, for example ${first.source_key}: ${first.error.replace(/[.\s]+$/, '')}` : '';
    const why = kept > 0 ? ' A batch with a failed copy keeps all its sources.' : '';
    return {
      type: 'warning',
      text: `${count(went)} of ${items} ${past}. ${count(o.failed)} failed${example}.${keptText}${why}`,
    };
  }
  if (o.total === 0) return { type: 'info', text: `Nothing to ${o.action}.` };
  if (kept > 0) return { type: 'warning', text: `${count(went)} of ${items} ${past}.${keptText}` };
  return { type: 'success', text: `${count(went)} ${noun(went, unit)} ${past}` };
}
