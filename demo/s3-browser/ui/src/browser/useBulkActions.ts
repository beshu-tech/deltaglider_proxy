import { useState, useEffect, useCallback, useRef } from 'react';
import { message } from 'antd';
import { useQueryClient } from '@tanstack/react-query';
import { qk } from '../queries/keys';
import { getBucket } from '../s3client';
import { bulkCopyObjects, startDeleteJob, bulkMoveObjects, bulkZipDownloadUrl, listAllUnderPrefix } from '../adminApi';
import { isSessionExpired } from '../errorHandling';
import { type ExpandedItem, MAX_BULK_OBJECTS, expandSelection } from '../bulkSelection';
import { type BulkAction, type BulkProgress, BulkActionFailed } from '../bulkBatches';
import type { DeleteJobOutcome, DeletePlanSummary } from '../bulkDelete';
import { type BulkTransferOutcome, transferInBatches, transferPlanError } from '../bulkTransfer';
import { downloadZip as saveZip, zipPreflightError } from '../zipDownload';

interface BulkOptions {
  selectedKeys: Set<string>;
  clearSelection: () => void;
  /** Reload the listing only (after each batch of a move). */
  refresh: () => void;
  /** Reload the listing and the savings chip (once, when a run ends). */
  mutate: () => void;
}

/**
 * Bulk delete / copy / move / ZIP of the selection through the admin objects
 * API. App gates them on `sessionCaps.canUseBulkActions` (the server
 * authorizes each key for a files-only session).
 */
export function useBulkActions({ selectedKeys, clearSelection, refresh, mutate }: BulkOptions) {
  // A running bulk delete, copy or move (null = none): its phase and counts for the action bar.
  const [bulkProgress, setBulkProgress] = useState<BulkProgress | null>(null);
  const bulkRun = useRef<AbortController | null>(null);
  const queryClient = useQueryClient();

  /**
   * Expand the selection against `currentBucket` via the ONE shared expander
   * (a few folders listed at once). Throws (before any mutation) when a folder
   * has more keys than the server lists, so no bulk action runs on a partial
   * folder.
   */
  const expandSelected = useCallback(
    (currentBucket: string, options?: Parameters<typeof expandSelection>[2]) =>
      expandSelection(selectedKeys, (pfx, after) => listAllUnderPrefix(currentBucket, pfx, after), options),
    [selectedKeys],
  );

  /**
   * One bulk run (delete, copy, move), the v2.0.4 bulk-delete shape: list the
   * selection's folders (a few at once), then send the items in batches, one
   * request at a time. The action bar shows `bulkProgress`; `cancelBulk`
   * stops the run after the batch in flight. Resolves with the outcome (also
   * after a cancel); rejects with a session expiry as-is (the bar sends the
   * user to sign-in), and with any other error plus how many objects went
   * before it. The listing and the savings chip reload once when it ends.
   */
  const runBulk = useCallback(
    async <O extends { cancelled: boolean; failed: number }>(
      action: BulkAction,
      run: {
        /** List the selection; `opts` carries the abort and the listing progress. */
        expand: (opts: NonNullable<Parameters<typeof expandSelection>[2]>) => Promise<ExpandedItem[]>;
        /**
         * Send the items in batches; `sent(done, went)` after each one, and
         * `progress(done, went)` as the items of a batch end (no reload).
         */
        send: (
          items: ExpandedItem[],
          signal: AbortSignal,
          sent: (done: number, went: number) => void,
          progress: (done: number, went: number) => void,
        ) => Promise<O>;
        /** The outcome of a run cancelled while it listed (or declined at the confirmation). */
        cancelledWhileListing: () => O;
        /** Asked after the listing, before the first request: `false` ends the run with nothing done. */
        confirm?: (items: ExpandedItem[]) => Promise<boolean>;
        /** Reload the listing after each batch: the sources leave the page as the run goes on. */
        reloadPerBatch: boolean;
      },
    ): Promise<O> => {
      const ctl = new AbortController();
      bulkRun.current = ctl;
      const stopping = () => ctl.signal.aborted;
      let total = 0;
      let went = 0;
      setBulkProgress({ action, phase: 'listing', listed: 0, folders: 0, keysFound: 0, stopping: false });
      try {
        const items = await run.expand({
          signal: ctl.signal,
          onProgress: (p) => setBulkProgress({ action, phase: 'listing', ...p, stopping: stopping() }),
        });
        if (run.confirm) {
          setBulkProgress({ action, phase: 'confirming', total: items.length, stopping: false });
          if (!(await run.confirm(items))) return run.cancelledWhileListing();
        }
        total = items.length;
        const startedAt = Date.now();
        setBulkProgress({ action, phase: 'sending', done: 0, total, stopping: stopping(), startedAt });
        const show = (done: number, n: number) => {
          went = n;
          setBulkProgress({ action, phase: 'sending', done, total, stopping: stopping(), startedAt });
        };
        const outcome = await run.send(
          items,
          ctl.signal,
          (done, n) => {
            show(done, n);
            if (run.reloadPerBatch) refresh();
          },
          show,
        );
        // A cancelled or partly failed run keeps the selection for a retry; the
        // reloads already dropped the entries that are gone.
        if (!outcome.cancelled && outcome.failed === 0) clearSelection();
        return outcome;
      } catch (e) {
        if (ctl.signal.aborted && e === ctl.signal.reason) return run.cancelledWhileListing();
        if (isSessionExpired(e)) throw e;
        throw new BulkActionFailed(action, e, went, total);
      } finally {
        bulkRun.current = null;
        setBulkProgress(null);
        // Once for the whole run (a failed batch may have done part of its
        // work): the listing and the savings chip, not after each batch.
        if (total > 0) mutate();
      }
    },
    [clearSelection, refresh, mutate],
  );

  /**
   * Delete the selection (see runBulk): count the objects, ask `confirm`
   * with the exact numbers, then start a server job that deletes the
   * selected folders and objects. The run ends when the job starts: the
   * delete goes on without this tab, and the bucket status shows it.
   */
  const bulkDelete = useCallback(
    (confirm?: (plan: DeletePlanSummary) => Promise<boolean>): Promise<DeleteJobOutcome> => {
      // Snapshot the bucket: a bucket switch mid-run must not move the delete.
      const bucket = getBucket();
      const selection = [...selectedKeys];
      const prefixes = [
        ...new Set(selection.filter((k) => k.startsWith('folder:') && k !== 'folder:').map((k) => k.slice('folder:'.length))),
      ];
      const keys = selection.filter((k) => !k.startsWith('folder:'));
      return runBulk<DeleteJobOutcome>('delete', {
        expand: (opts) => expandSelected(bucket, opts),
        confirm:
          confirm &&
          ((items) =>
            items.length === 0
              ? Promise.resolve(true)
              : confirm({ objects: items.length, folders: prefixes.length, files: keys.length })),
        send: async (items) => {
          if (items.length === 0) return { total: 0, cancelled: false, failed: 0 };
          const { job_id } = await startDeleteJob({ bucket, prefixes, keys, expected: items.length });
          // The banner shows the job at once, not at the next idle poll.
          void queryClient.invalidateQueries({ queryKey: qk.maintenance.bucket(bucket) });
          return { total: items.length, cancelled: false, failed: 0, jobId: job_id };
        },
        cancelledWhileListing: () => ({ total: 0, cancelled: true, failed: 0 }),
        reloadPerBatch: false,
      });
    },
    [runBulk, expandSelected, selectedKeys, queryClient],
  );

  /**
   * Copy or move the selection to `destBucket`/`destPrefix` (see runBulk).
   * A selected folder `foo/` keeps its own name under the destination
   * (`foo/bar/a.txt` lands at `destPrefix + foo/bar/a.txt`); a selected object
   * lands under its basename. The listing stops once the selection passes
   * the server's cap (MAX_BULK_OBJECTS), and the whole plan is checked before
   * the first batch: the server sees one batch at a time. For a move, the
   * server removes a batch's sources only when every copy of that batch
   * succeeded.
   */
  const bulkTransfer = useCallback(
    (action: 'copy' | 'move', destBucket: string, destPrefix: string): Promise<BulkTransferOutcome> => {
      // Snapshot the source bucket ONCE so the listing and the requests run
      // against the same bucket even if the user switches buckets mid-run.
      const sourceBucket = getBucket();
      const request = action === 'copy' ? bulkCopyObjects : bulkMoveObjects;
      return runBulk<BulkTransferOutcome>(action, {
        expand: async (opts) => {
          // The folder's own marker key (empty suffix) is not copied.
          const items = (await expandSelected(sourceBucket, { ...opts, maxKeys: MAX_BULK_OBJECTS })).filter(
            (i) => i.relative !== '',
          );
          const problem = transferPlanError(items, sourceBucket, destBucket, destPrefix);
          if (problem) throw new Error(problem);
          return items;
        },
        send: (items, signal, sent) =>
          transferInBatches(
            action,
            items,
            (batch) =>
              request({
                source_bucket: sourceBucket,
                dest_bucket: destBucket,
                dest_prefix: destPrefix,
                items: batch.map(({ source, relative }) => ({ source_key: source, relative })),
              }),
            { signal, onBatch: (p) => sent(p.done, p.went) },
          ),
        cancelledWhileListing: () => ({
          action,
          total: 0,
          succeeded: 0,
          failed: 0,
          deleted: 0,
          failures: [],
          cancelled: true,
        }),
        reloadPerBatch: action === 'move',
      });
    },
    [runBulk, expandSelected],
  );
  const bulkCopy = useCallback(
    (destBucket: string, destPrefix: string) => bulkTransfer('copy', destBucket, destPrefix),
    [bulkTransfer],
  );
  const bulkMove = useCallback(
    (destBucket: string, destPrefix: string) => bulkTransfer('move', destBucket, destPrefix),
    [bulkTransfer],
  );

  /** Stop the running bulk action: the batch in flight finishes, no new one starts. */
  const cancelBulk = useCallback(() => {
    bulkRun.current?.abort();
    setBulkProgress((p) => (p ? { ...p, stopping: true } : p));
  }, []);

  // Leaving the page stops the run after the batch in flight: ask first.
  const runningAction = bulkProgress?.action;
  useEffect(() => {
    if (!runningAction) return;
    const warn = (e: BeforeUnloadEvent) => {
      e.preventDefault();
      e.returnValue = `A bulk ${runningAction} is running. Leaving the page stops it.`;
      return e.returnValue;
    };
    window.addEventListener('beforeunload', warn);
    return () => window.removeEventListener('beforeunload', warn);
  }, [runningAction]);

  const downloadZip = useCallback(async () => {
    // The proxy builds the archive; the browser resolves the selection and
    // saves the response. See zipDownload.ts for how failures stay visible.
    const bucket = getBucket();
    // The listing stops once the selection passes what one ZIP takes.
    const keys = (await expandSelected(bucket, { maxKeys: MAX_BULK_OBJECTS })).map((i) => i.source);
    const url = bulkZipDownloadUrl(bucket, keys);
    const blocked = zipPreflightError(keys.length, url.length);
    if (blocked) throw new Error(blocked);
    const filename = `deltaglider-${new Date().toISOString().slice(0, 10)}.zip`;
    const outcome = await saveZip(url, filename);
    if (outcome === 'saved') message.success(`Saved ${filename}`);
    if (outcome === 'started') {
      message.info(
        "The ZIP download started. If it fails, your browser's download list shows it.",
        8,
      );
    }
  }, [expandSelected]);

  return {
    bulkDelete,
    bulkCopy,
    bulkMove,
    cancelBulk,
    downloadZip,
    bulkRunning: bulkProgress !== null,
    bulkProgress,
  };
}
