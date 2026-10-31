/**
 * Pure text and state of a bucket's conversion to encryption
 * (docs/plan/encryption-conversion-marker.md). The server opens a
 * conversion window when an apply makes a bucket that holds objects
 * encrypt, and queues the re-encrypt job; clients read the plaintext
 * history until the job closes the window. React-free, so the unit
 * tests read the exact words.
 */
import type { ConversionPreview, ConversionView } from './adminApi';

/** "The 1 object" / "The 1,204 objects" / "More than 10,000 objects". */
function objectsText(n: number, more: boolean): string {
  const count = n.toLocaleString('en-US');
  if (more) return `More than ${count} objects`;
  return n === 1 ? 'The 1 object' : `The ${count} objects`;
}

/**
 * The sentence of the apply dialog: how many objects the job encrypts in
 * the background, and that they stay readable meanwhile. `null` when the
 * change starts no conversion.
 */
export function conversionCountText(previews: readonly ConversionPreview[] | undefined): string | null {
  if (!previews || previews.length === 0) return null;
  const total = previews.reduce((n, p) => n + p.objects, 0);
  const more = previews.some((p) => p.more);
  const where =
    previews.length === 1 ? `bucket ${previews[0].bucket}` : `${previews.length} buckets (${previews.map((p) => p.bucket).join(', ')})`;
  const objects = objectsText(total, more);
  const verb = !more && total === 1 ? 'is' : 'are';
  return `${objects} already in ${where} ${verb} not encrypted yet. After you apply, a re-encrypt job encrypts them in the background. They stay readable while the job runs.`;
}

/** The versioning note of each versioned bucket in `previews`, in plain words. */
export function versionedWarnings(previews: readonly ConversionPreview[] | undefined): string[] {
  return (previews ?? [])
    .filter((p) => p.versioned)
    .map(
      (p) =>
        `The backend keeps old versions of bucket ${p.bucket}. The job encrypts the current objects; their old unencrypted versions stay on the backend.`,
    );
}

/**
 * The server's warnings without its versioning note of each versioned
 * bucket in `previews`: the dialog says it once, in its own words (the
 * server's text names an HTTP header, for API clients).
 */
export function withoutVersionedNotes(
  warnings: readonly string[] | undefined,
  previews: readonly ConversionPreview[] | undefined,
): string[] {
  const notes = (previews ?? []).filter((p) => p.versioned).map((p) => `bucket '${p.bucket}' keeps old versions`);
  return (warnings ?? []).filter((w) => !notes.some((n) => w.startsWith(n)));
}

/** The label of the box that asks the job to delete the plaintext versions. */
export const PURGE_LABEL = 'Also delete the old unencrypted versions';

/** What the box does, in plain words: shown under it. */
export const PURGE_EXPLANATION =
  'Deletes the old unencrypted versions of these objects after the job encrypts them. This cannot be undone; encrypted versions and current objects are kept.';

/** Does the dialog offer the purge box? Only for a versioned bucket. */
export function offersPurge(previews: readonly ConversionPreview[] | undefined): boolean {
  return (previews ?? []).some((p) => p.versioned);
}

/** What the conversion row of a bucket shows. */
export type ConversionRowState =
  /** The job of this node runs: progress. */
  | { kind: 'running'; jobId: number; percent: number | null }
  /** The window is open and no job runs here: Resume. `ended` is how the
   *  latest job of this node ended (`none`: no job here, or it completed). */
  | { kind: 'resume'; jobId: number | null; ended: 'none' | 'failed' | 'cancelled'; deadline: number | null }
  /** The deadline passed before the job finished: clients get errors. */
  | { kind: 'stopped'; jobId: number | null };

const ACTIVE = new Set(['queued', 'running', 'cancelling']);

/** The row of a bucket's conversion; `null` = no conversion marker. */
export function conversionRowState(view: ConversionView | null | undefined): ConversionRowState | null {
  if (!view?.run) return null;
  const job = view.job_on_this_node ?? null;
  if (!view.open) return { kind: 'stopped', jobId: job?.id ?? null };
  if (job && ACTIVE.has(job.status)) return { kind: 'running', jobId: job.id, percent: job.percent ?? null };
  const ended = !job || job.status === 'completed' ? 'none' : job.status === 'cancelled' ? 'cancelled' : 'failed';
  return { kind: 'resume', jobId: job?.id ?? null, ended, deadline: view.deadline ?? null };
}

/** A deadline (unix seconds) as the UI writes it ("Nov 7, 2026, 10:00 AM"
 *  in the browser's locale). */
export function deadlineText(seconds: number): string {
  return new Date(seconds * 1000).toLocaleString(undefined, { dateStyle: 'medium', timeStyle: 'short' });
}

/** The objects the job encrypted although they were stored after
 *  encryption started (clients could not read them as plaintext). */
export function adoptedText(n: number): string {
  return n === 1
    ? '1 object was stored after encryption started; it was encrypted too'
    : `${n.toLocaleString('en-US')} objects were stored after encryption started; they were encrypted too`;
}

/**
 * The words of a conversion state, as a continuation of a status line
 * ("Now: Encrypted · bucket key k · <phrase>"): lower case, no full stop.
 * [`sentence`] makes it a sentence of its own.
 */
export function conversionPhrase(state: ConversionRowState): string {
  switch (state.kind) {
    case 'running':
      return state.percent === null
        ? 'starting to encrypt the existing objects…'
        : `encrypting the existing objects, ${state.percent}%`;
    case 'resume': {
      const what =
        state.ended === 'failed'
          ? 'the re-encrypt job stopped with failures'
          : state.ended === 'cancelled'
            ? 'the re-encrypt job was cancelled'
            : // Job state is per proxy: another proxy of a cluster may run it.
              'some existing objects are not encrypted yet, and this proxy runs no re-encrypt job for them';
      const until = state.deadline === null ? '' : ` until ${deadlineText(state.deadline)}`;
      return `${what}; unencrypted objects stay readable${until}`;
    }
    case 'stopped':
      return 'the conversion stopped before it finished; objects that are not encrypted yet fail to read';
  }
}

/** A phrase as a sentence: capital first letter, full stop (not after "…"). */
export function sentence(phrase: string): string {
  const s = phrase.charAt(0).toUpperCase() + phrase.slice(1);
  return /[.…]$/.test(s) ? s : `${s}.`;
}

/** How often the bucket list refetches while a conversion job runs. */
export const CONVERSION_POLL_MS = 3_000;

/** The refetch of a conversion whose deadline passed on this clock but
 *  not yet on the backend's (the backend clock decides). */
const DEADLINE_RETRY_MS = 60_000;
const DAY_MS = 86_400_000;

/**
 * When the bucket list refetches on its own, in ms; `false` = never.
 *
 *   * A job runs here: every 3 s (its progress, then the close).
 *   * The conversion waits for Resume: once, just after the earliest
 *     deadline, when it turns into "stopped". Nothing else changes it
 *     without a click: Resume refetches the list, another tab refetches
 *     on window focus, and a job on another proxy is not visible here
 *     (job state is per proxy). A 3 s poll for up to 7 days per admin
 *     tab bought nothing.
 *   * Stopped, or no conversion: never.
 */
export function conversionPollMs(
  buckets: ReadonlyArray<{ effective_encryption?: { conversion?: ConversionView } | null }> | undefined,
  nowMs: number,
): number | false {
  let next: number | false = false;
  for (const b of buckets ?? []) {
    const s = conversionRowState(b.effective_encryption?.conversion);
    if (s?.kind === 'running') return CONVERSION_POLL_MS;
    if (s?.kind === 'resume' && s.deadline !== null) {
      const ms = s.deadline * 1000 - nowMs + 1_000;
      // A day at most: setInterval overflows past 2^31 ms (about 24 days).
      const at = ms > 0 ? Math.min(Math.max(ms, CONVERSION_POLL_MS), DAY_MS) : DEADLINE_RETRY_MS;
      next = next === false ? at : Math.min(next, at);
    }
  }
  return next;
}

/** The id of a re-encrypt job in the Jobs page (`?job=`). */
export function runId(jobId: number): string {
  return `maintenance:${jobId}`;
}
