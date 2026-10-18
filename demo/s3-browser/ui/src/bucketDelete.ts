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

/**
 * Why the proxy refused a DeleteBucket with `BucketNotEmpty`, read from its
 * message (`src/s3_adapter_s3s/bucket.rs::delete_bucket`):
 * - `objects-remain`: a visible object remains. Its multipart-upload count
 *   is information only: the uploads do not block the delete, and they can
 *   be uploads that other users run now.
 * - `finalising`: no object remains, but some multipart uploads are being
 *   completed now. A retry in a few seconds can succeed. (When no object
 *   remains and no upload is completing, the proxy removes the uploads
 *   itself and deletes the bucket.)
 * - `not-empty`: a `BucketNotEmpty` in another shape (another server).
 * `null` when the error is not a `BucketNotEmpty`.
 */
export type BucketDeleteRefusal =
  | { kind: 'objects-remain'; exampleKey?: string; uploads: number }
  | { kind: 'finalising'; uploads: number }
  | { kind: 'not-empty' };

export function classifyBucketDeleteRefusal(code: string | undefined, message: string): BucketDeleteRefusal | null {
  if (code !== 'BucketNotEmpty' && !/BucketNotEmpty/i.test(message)) return null;
  const finalising = message.match(/blocked: (\d+) multipart upload\(s\) finalising/i);
  if (finalising) return { kind: 'finalising', uploads: Number.parseInt(finalising[1], 10) };
  if (/blocked: visible object remains/i.test(message)) {
    const example = message.match(/example_key=(.*), multipart_uploads=\d+/s);
    const uploads = message.match(/multipart_uploads=(\d+)/i);
    return {
      kind: 'objects-remain',
      exampleKey: example ? example[1] : undefined,
      uploads: uploads ? Number.parseInt(uploads[1], 10) : 0,
    };
  }
  return { kind: 'not-empty' };
}

function uploadsAre(n: number): string {
  return `${n} multipart upload${n === 1 ? ' is' : 's are'}`;
}

/** The message that the sidebar shows for a refused delete. */
export function bucketDeleteRefusalText(bucket: string, refusal: BucketDeleteRefusal): string {
  switch (refusal.kind) {
    case 'objects-remain': {
      const example = refusal.exampleKey ? `, for example "${refusal.exampleKey}"` : '';
      const uploads = refusal.uploads > 0 ? ` ${uploadsAre(refusal.uploads)} also in progress in it.` : '';
      return `Bucket "${bucket}" still holds objects${example}. Delete the objects first, and then delete the bucket.${uploads}`;
    }
    case 'finalising':
      return `${uploadsAre(refusal.uploads)} finishing in bucket "${bucket}". Try to delete the bucket again in a few seconds.`;
    case 'not-empty':
      return `Bucket "${bucket}" is not empty. Delete its objects first, and then delete the bucket.`;
  }
}
