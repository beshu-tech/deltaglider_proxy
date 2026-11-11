import { useState, useEffect, useCallback, useRef } from 'react';
import { message } from 'antd';
import { useQueryClient } from '@tanstack/react-query';
import { qk } from '../queries/keys';
import { getBucket, listDirectKeys } from '../s3client';
import { bulkCopyObjects, startDeleteJob, bulkMoveObjects, bulkZipDownloadUrl, listAllUnderPrefix } from '../adminApi';
import { isSessionExpired } from '../errorHandling';
import { type ExpandedItem, MAX_BULK_OBJECTS, expandSelection } from '../bulkSelection';
import { type RowKey, splitRowKeys } from '../rowKey';
import { type BulkAction, type BulkProgress, BulkActionFailed } from '../bulkBatches';
import type { DeleteJobOutcome, DeletePlanSummary } from '../bulkDelete';
import {
  type BulkTransferOutcome,
  type TransferBatchResult,
  mergeBatchResults,
  transferInBatches,
  transferPlanError,
} from '../bulkTransfer';
import { type ConflictResolver, findConflicts, overwriteSources } from '../bulkConflicts';
import { downloadZip as saveZip, zipPreflightError } from '../zipDownload';

interface BulkOptions {
  selectedKeys: Set<RowKey>;
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
        /**
         * Asked after the listing, before the first request: look at the
         * destination and settle what to send. The items to send, or `null`
         * to end the run with nothing done. `ctl` shows the phase in the bar.
         */
        prepare?: (
          items: ExpandedItem[],
          ctl: {
            signal: AbortSignal;
            checking: (checked: number, total: number) => void;
            confirming: (total: number) => void;
          },
        ) => Promise<ExpandedItem[] | null>;
        /** The outcome of a run cancelled while it listed (or declined at the confirmation). */
        cancelledWhileListing: () => O;
        /** Asked after the listing, before the first request: `false` ends the run with nothing done. */
        confirm?: (items: ExpandedItem[]) => Promise<boolean>;
        /** Reload the listing after each batch: the sources leave the page as the run goes on. */
        reloadPerBatch: boolean;
        /** The buckets whose size and object count the run changes. */
        buckets: string[];
      },
    ): Promise<O> => {
      const ctl = new AbortController();
      bulkRun.current = ctl;
      const stopping = () => ctl.signal.aborted;
      let total = 0;
      let went = 0;
      setBulkProgress({ action, phase: 'listing', listed: 0, folders: 0, keysFound: 0, stopping: false });
      try {
        let items = await run.expand({
          signal: ctl.signal,
          onProgress: (p) => setBulkProgress({ action, phase: 'listing', ...p, stopping: stopping() }),
        });
        if (run.prepare) {
          const prepared = await run.prepare(items, {
            signal: ctl.signal,
            checking: (checked, n) => setBulkProgress({ action, phase: 'checking', checked, total: n, stopping: stopping() }),
            confirming: (n) => setBulkProgress({ action, phase: 'confirming', total: n, stopping: false }),
          });
          if (prepared === null) return run.cancelledWhileListing();
          items = prepared;
        }
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
        // The header's bucket totals of every bucket the run wrote to, also
        // a copy's destination bucket, which no listing here reloads.
        if (total > 0) {
          mutate();
          for (const b of new Set(run.buckets)) void queryClient.invalidateQueries({ queryKey: qk.bucketUsage(b) });
        }
      }
    },
    [clearSelection, refresh, mutate, queryClient],
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
      const { prefixes, keys } = splitRowKeys(selectedKeys);
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
        buckets: [bucket],
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
   *
   * Nothing is written over an existing object unasked (F17), and the
   * SERVER keeps that promise: every item that the user did not allow to
   * replace an object goes with `no_overwrite`, which the server judges
   * under the destination's write lock (so an object that appears after the
   * check, or one this user cannot list, is never replaced). After the
   * listing, the destination folders are listed once each as a preview, and
   * when an existing destination turns up `onConflicts` decides: skip (those
   * items go conditionally and the server leaves them; a move then keeps
   * their sources), overwrite (only those items go unconditionally), or
   * cancel. With no `onConflicts` an existing destination cancels the run.
   * A destination folder that cannot be listed does not fail the run: its
   * items go conditionally too, and the result says how many were skipped.
   */
  const bulkTransfer = useCallback(
    (
      action: 'copy' | 'move',
      destBucket: string,
      destPrefix: string,
      onConflicts?: ConflictResolver,
    ): Promise<BulkTransferOutcome> => {
      // Snapshot the source bucket ONCE so the listing and the requests run
      // against the same bucket even if the user switches buckets mid-run.
      const sourceBucket = getBucket();
      const request = action === 'copy' ? bulkCopyObjects : bulkMoveObjects;
      // The sources the user allowed to replace an existing object.
      let replace = new Set<string>();
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
        prepare: async (items, { signal, checking, confirming }) => {
          const { conflicts } = await findConflicts(
            items,
            sourceBucket === destBucket,
            destPrefix,
            (folder, after) => listDirectKeys(destBucket, folder, after),
            { signal, onProgress: (p) => checking(p.checked, p.total) },
          );
          if (conflicts.length === 0) return items;
          confirming(items.length);
          const choice = onConflicts
            ? await onConflicts({ action, destBucket, total: items.length, conflicts })
            : 'cancel';
          if (choice === 'cancel') return null;
          replace = overwriteSources(conflicts, choice);
          return items;
        },
        send: async (items, signal, sent) =>
          transferInBatches(
            action,
            items,
            async (batch) => {
              const body = (part: typeof batch, noOverwrite: boolean) => ({
                source_bucket: sourceBucket,
                dest_bucket: destBucket,
                dest_prefix: destPrefix,
                items: part.map(({ source, relative }) => ({ source_key: source, relative })),
                no_overwrite: noOverwrite,
              });
              const mayReplace = batch.filter((i) => replace.has(i.source));
              const mayNot = batch.filter((i) => !replace.has(i.source));
              const answers: TransferBatchResult[] = [];
              if (mayNot.length > 0) answers.push(await request(body(mayNot, true)));
              if (mayReplace.length > 0) answers.push(await request(body(mayReplace, false)));
              return mergeBatchResults(answers);
            },
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
        buckets: [sourceBucket, destBucket],
      });
    },
    [runBulk, expandSelected],
  );
  const bulkCopy = useCallback(
    (destBucket: string, destPrefix: string, onConflicts?: ConflictResolver) =>
      bulkTransfer('copy', destBucket, destPrefix, onConflicts),
    [bulkTransfer],
  );
  const bulkMove = useCallback(
    (destBucket: string, destPrefix: string, onConflicts?: ConflictResolver) =>
      bulkTransfer('move', destBucket, destPrefix, onConflicts),
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
