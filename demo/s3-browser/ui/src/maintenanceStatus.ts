/**
 * Pure status→UI mapping for bucket maintenance (re-encryption) jobs.
 *
 * React-free so the Node regression script can transpile-and-import it
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

/** Short human label for the active phase, used next to the progress bar. */
export function phaseLabel(job: MaintenanceJobView): string {
  if (job.status === 'queued') return 'Waiting to start…';
  if (job.status === 'cancelling') return 'Cancelling…';
  switch (job.phase) {
    case 'counting':
      return 'Counting objects…';
    case 'references':
      return 'Finalizing shared reference files…';
    default: {
      const total = job.objects_total;
      const processed = job.objects_done + job.objects_skipped;
      return total != null ? `${processed} / ${total} objects` : `${processed} objects`;
    }
  }
}

/** One-line banner text for the object browser (kind-aware). */
export function browserBannerText(job: MaintenanceJobView): string {
  const pct = activePercent(job);
  const pctPart = pct != null ? ` — ${pct}%` : '';
  const verb = job.kind === 'migrate' ? 'Migrating this bucket to another backend' : 'Re-encrypting this bucket';
  return `${verb}${pctPart}. Files stay readable; uploads and deletes are temporarily unavailable.`;
}

