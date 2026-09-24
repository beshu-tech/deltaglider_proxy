/**
 * Pure rules for the sidebar's "Delete bucket" entry. The row menu probes
 * the bucket with one small listing; these turn the probe into the menu
 * label and the confirm text.
 */

/** Objects the probe lists at most: enough to say "not empty", cheap to ask. */
export const BUCKET_PROBE_CAP = 100;
/** A probe that has not answered by then counts as failed. */
export const BUCKET_PROBE_TIMEOUT_MS = 8000;

export type BucketObjectCount =
  | { state: 'checking' }
  | { state: 'known'; count: number; truncated: boolean }
  | { state: 'error' };

/** The row menu's Delete entry: its label, and whether it can be used. */
export function deleteMenuEntry(probe: BucketObjectCount | undefined): { label: string; disabled: boolean } {
  if (!probe || probe.state === 'checking') return { label: 'Delete bucket (checking contents…)', disabled: true };
  if (probe.state === 'error') return { label: 'Delete bucket…', disabled: false };
  if (probe.count === 0) return { label: 'Delete bucket…', disabled: false };
  const n = probe.truncated ? `${probe.count}+` : String(probe.count);
  return { label: `Delete bucket (not empty: ${n} object${n === '1' ? '' : 's'})`, disabled: true };
}

/** The confirm text. Only a probe that answered "0 objects" may say "empty". */
export function deleteConfirmText(probe: BucketObjectCount | undefined): string {
  if (probe?.state === 'known' && probe.count === 0) return 'The bucket is empty. Deleting it cannot be undone.';
  return 'The contents of this bucket could not be checked. The proxy deletes only an empty bucket, and deleting it cannot be undone.';
}
