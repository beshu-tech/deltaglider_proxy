/**
 * Pure status→UI mapping for bucket maintenance (re-encryption) jobs.
 *
 * React-free so the unit test can import it
 * directly (same pattern as droppedFiles.ts / destinationSuggest.ts).
 *
 * The server's job view carries `percent` (0-99 while running, null
 * while counting, 100 on completed). These helpers turn that into the
 * three UI surfaces: the Buckets-page chip, the row progress bar, and
 * the browser banner.
 */

export interface MaintenanceJobView {
  id: number;
  kind: string;
  bucket: string;
  status: string; // queued | running | cancelling | completed | failed | cancelled
  phase: string; // counting | objects | references
  objects_total: number | null;
  objects_done: number;
  objects_skipped: number;
  objects_failed: number;
  bytes_done: number;
  percent: number | null;
  /**
   * Whether the proxy refuses writes to the bucket right now (newer servers
   * send it). A migration that is cancelled before the switch, or that is in
   * its clean-up after the switch, no longer blocks writes.
   */
  writes_blocked?: boolean;
  // No last_error: this projection is readable by non-admin browser
  // sessions; error detail is admin-tier only (the jobs API).
  triggered_by: string | null;
  created_at: number;
  started_at: number | null;
  finished_at: number | null;
}

/**
 * Progress-bar percent for an ACTIVE job. `null` = indeterminate (queued,
 * or still counting objects) — render an animated/indeterminate bar.
 */
export function activePercent(job: MaintenanceJobView): number | null {
  if (job.status === 'queued') return null;
  return job.percent;
}

/**
 * Whether uploads and deletes to the bucket are refused now. This is the
 * server's answer (`writes_blocked`, the state of its write gate), never a
 * guess from the job's kind or phase: a cancelled migration opens the
 * bucket while its row is still active, and a migration that cleans up
 * after the switch does not gate it. A server without the field gated the
 * bucket for every active job.
 */
export function bucketWritesBlocked(
  status: { active: MaintenanceJobView | null; writes_blocked?: boolean } | null | undefined,
): boolean {
  if (!status) return false;
  if (typeof status.writes_blocked === 'boolean') return status.writes_blocked;
  if (status.active && typeof status.active.writes_blocked === 'boolean') return status.active.writes_blocked;
  return status.active != null;
}

/** Short human label for the active phase, used next to the progress bar. */
export function phaseLabel(job: MaintenanceJobView): string {
  if (job.status === 'queued') return 'Waiting to start…';
  if (job.status === 'cancelling') return 'Cancelling…';
  const processed = job.objects_done + job.objects_skipped;
  switch (job.phase) {
    case 'counting':
      return 'Counting objects…';
    case 'references':
      return 'Finalizing shared reference files…';
    case 'purge-versions':
      return 'Deleting the old unencrypted versions…';
    case 'stage':
      return 'Preparing the target…';
    case 'copy':
      return `Copying · ${processed.toLocaleString()} objects`;
    case 'verify':
      return 'Verifying the copies…';
    case 'flip':
      return 'Switching to the new backend…';
    case 'cleanup':
      return 'Deleting the old copies…';
    default: {
      const total = job.objects_total;
      return total != null
        ? `${processed.toLocaleString()} / ${total.toLocaleString()} objects`
        : `${processed.toLocaleString()} objects`;
    }
  }
}

/**
 * One-line banner text for the object browser (kind-aware). Whether it
 * says that uploads work follows `writesBlocked` (the server's answer).
 */
export function browserBannerText(job: MaintenanceJobView, writesBlocked: boolean): string {
  const noun = job.kind === 'migrate' ? 'migration' : job.kind === 'backfill-metadata' ? 'metadata backfill' : 're-encryption';
  if (job.status === 'cancelling') {
    return writesBlocked
      ? `The ${noun} of this bucket is being cancelled. Files stay readable; uploads and deletes work again when it stops.`
      : `The ${noun} of this bucket is being cancelled. Uploads and deletes work again.`;
  }
  if (!writesBlocked && job.kind === 'migrate' && job.phase === 'cleanup') {
    return 'This bucket now uses its new backend. The proxy is deleting the old copies; uploads and deletes work.';
  }
  const pct = activePercent(job);
  const pctPart = pct != null ? ` — ${pct}%` : '';
  const verb =
    job.kind === 'migrate'
      ? 'Migrating this bucket to another backend'
      : job.kind === 'backfill-metadata'
        ? 'Adding metadata to the objects of this bucket'
        : 'Re-encrypting this bucket';
  const tail = writesBlocked
    ? 'Files stay readable; uploads and deletes are temporarily unavailable.'
    : 'Files stay readable, and uploads and deletes work.';
  return `${verb}${pctPart}. ${tail}`;
}

/**
 * Poll period of the browser's bucket-status query. Fast while a job runs;
 * the idle floor otherwise. After an error (an expired session answers
 * 401) the poll slows down but does not stop: once the user signs in again
 * the banner comes back without a reload.
 */
export function bucketStatusPollMs(
  hasActive: boolean,
  failed: boolean,
  activeMs: number,
  idleMs: number,
): number {
  if (failed) return idleMs * 3;
  return hasActive ? activeMs : idleMs;
}

/**
 * The banner line of one running delete job: what it did of how much, and
 * who started it when that is not the reader.
 */
export function deleteJobText(job: MaintenanceJobView, me: string | null): string {
  if (job.status === 'cancelling') return 'Stopping the delete…';
  if (job.status === 'queued' && job.started_at == null) return 'Delete waiting to start…';
  if (job.objects_total == null) return 'Starting the delete…';
  const done = job.objects_done + job.objects_failed;
  const total = job.objects_total;
  const pct = total > 0 ? ` · ${Math.min(99, Math.floor((done / total) * 100))}%` : '';
  const who = job.triggered_by && job.triggered_by !== me ? ` · started by ${job.triggered_by}` : '';
  const failed = job.objects_failed > 0 ? ` · ${job.objects_failed.toLocaleString('en-US')} failed` : '';
  return `Deleting ${done.toLocaleString('en-US')} of ${total.toLocaleString('en-US')} objects${pct}${failed}${who}`;
}
