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

/** The versioning warning of each versioned bucket in `previews`. */
export function versionedWarnings(previews: readonly ConversionPreview[] | undefined): string[] {
  return (previews ?? [])
    .filter((p) => p.versioned)
    .map(
      (p) =>
        `The backend keeps old versions of bucket ${p.bucket}. The job encrypts the current objects, and their plaintext versions stay on the backend as noncurrent versions.`,
    );
}

/** The label of the box that asks the job to delete the plaintext versions. */
export const PURGE_LABEL = 'Also delete the old unencrypted versions';

/** What the box does, in plain words: shown under it. */
export const PURGE_EXPLANATION =
  'When the job has encrypted every object, it deletes the old unencrypted versions of those objects from the backend. They are deleted for good: nobody can restore them afterwards. Encrypted versions and the current objects are never deleted.';

/** Does the dialog offer the purge box? Only for a versioned bucket. */
export function offersPurge(previews: readonly ConversionPreview[] | undefined): boolean {
  return (previews ?? []).some((p) => p.versioned);
}

/** What the conversion row of a bucket shows. */
export type ConversionRowState =
  /** The job of this node runs: progress. */
  | { kind: 'running'; jobId: number; percent: number | null }
  /** The window is open and no job runs here (none, or it ended): Resume. */
  | { kind: 'resume'; jobId: number | null; failed: boolean }
  /** The deadline passed before the job finished: clients get errors. */
  | { kind: 'stopped'; jobId: number | null };

const ACTIVE = new Set(['queued', 'running', 'cancelling']);

/** The row of a bucket's conversion; `null` = no conversion marker. */
export function conversionRowState(view: ConversionView | null | undefined): ConversionRowState | null {
  if (!view?.run) return null;
  const job = view.job_on_this_node ?? null;
  if (!view.open) return { kind: 'stopped', jobId: job?.id ?? null };
  if (job && ACTIVE.has(job.status)) return { kind: 'running', jobId: job.id, percent: job.percent ?? null };
  return { kind: 'resume', jobId: job?.id ?? null, failed: !!job && job.status !== 'completed' };
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
    case 'resume':
      return state.failed
        ? 'the encryption of the existing objects stopped with failures; they stay readable until the window closes'
        : 'not every existing object is encrypted, and no job runs on this instance; they stay readable until the window closes';
    case 'stopped':
      return 'the conversion stopped before it finished; objects that are not encrypted yet fail to read';
  }
}

/** A phrase as a sentence: capital first letter, full stop (not after "…"). */
export function sentence(phrase: string): string {
  const s = phrase.charAt(0).toUpperCase() + phrase.slice(1);
  return /[.…]$/.test(s) ? s : `${s}.`;
}

/** Does a view in `buckets` need a refetch to follow its conversion? A
 *  running or resumable conversion changes without a click here (the job
 *  ends, or another tab resumes it); a stopped one does not. */
export function conversionNeedsPolling(
  buckets: ReadonlyArray<{ effective_encryption?: { conversion?: ConversionView } | null }> | undefined,
): boolean {
  return (buckets ?? []).some((b) => {
    const s = conversionRowState(b.effective_encryption?.conversion);
    return s !== null && s.kind !== 'stopped';
  });
}

/** How often the bucket list refetches while a conversion runs. */
export const CONVERSION_POLL_MS = 3_000;

/** The id of a re-encrypt job in the Jobs page (`?job=`). */
export function runId(jobId: number): string {
  return `maintenance:${jobId}`;
}
