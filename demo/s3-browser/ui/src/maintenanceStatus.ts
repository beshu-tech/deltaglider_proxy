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
 * Whether uploads and deletes to the bucket are refused while this job is
 * active. The server's own answer wins; without it: a migration that is
 * cancelling (its source was opened again) or cleaning up after the switch
 * does not block writes, every other active job does.
 */
export function bucketWritesBlocked(job: MaintenanceJobView | null): boolean {
  if (!job) return false;
  if (typeof job.writes_blocked === 'boolean') return job.writes_blocked;
  if (job.kind === 'migrate' && (job.status === 'cancelling' || job.phase === 'cleanup')) return false;
  return true;
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

/** One-line banner text for the object browser (kind-aware). */
export function browserBannerText(job: MaintenanceJobView): string {
  const pct = activePercent(job);
  const pctPart = pct != null && job.status !== 'cancelling' ? ` — ${pct}%` : '';
  if (job.kind === 'migrate' && job.status === 'cancelling') {
    return 'The migration of this bucket is being cancelled. The bucket stays on its current backend, and uploads and deletes work again.';
  }
  if (job.kind === 'migrate' && job.phase === 'cleanup') {
    return 'This bucket now uses its new backend. The proxy is deleting the old copies; uploads and deletes work.';
  }
  const verb =
    job.kind === 'migrate'
      ? 'Migrating this bucket to another backend'
      : job.kind === 'backfill-metadata'
        ? 'Adding metadata to the objects of this bucket'
        : 'Re-encrypting this bucket';
  const tail = bucketWritesBlocked(job)
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
