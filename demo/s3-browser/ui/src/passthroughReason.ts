/**
 * Why an object is stored whole ("Original" / passthrough), for the inspector.
 * Pure, and the same eligibility rule as the server's `FileRouter`: a key is
 * eligible when it ends with `.<ext>` for one of the bucket's file types (the
 * built-in list plus `delta_extensions.add`, minus `.remove`).
 */

export interface BucketDeltaPolicy {
  compression?: boolean | null;
  delta_extensions?: { add?: string[]; remove?: string[] };
}

/** The bucket's delta-eligible file types. */
export function bucketDeltaExtensions(defaults: readonly string[], policy?: BucketDeltaPolicy | null): string[] {
  const remove = new Set(policy?.delta_extensions?.remove ?? []);
  return [...new Set([...defaults, ...(policy?.delta_extensions?.add ?? [])])].filter((e) => !remove.has(e));
}

export function isDeltaEligible(key: string, extensions: readonly string[]): boolean {
  const lower = key.toLowerCase();
  return extensions.some((e) => lower.endsWith(`.${e}`));
}

export type PassthroughReason =
  /** The bucket has `compression: false`. */
  | { kind: 'compression-off' }
  /** The file type is not one the bucket tries as a delta. */
  | { kind: 'file-type'; ext: string | null }
  /** Eligible, so the delta was tried and was not small enough (or the
   *  upload asked for no delta). */
  | { kind: 'delta-not-kept' }
  /** No admin config to tell (a non-admin session). */
  | { kind: 'unknown' };

export function passthroughReason(
  key: string,
  defaults: readonly string[] | undefined,
  policy: BucketDeltaPolicy | null | undefined,
): PassthroughReason {
  if (policy?.compression === false) return { kind: 'compression-off' };
  if (!defaults || defaults.length === 0) return { kind: 'unknown' };
  if (isDeltaEligible(key, bucketDeltaExtensions(defaults, policy))) return { kind: 'delta-not-kept' };
  const name = key.slice(key.lastIndexOf('/') + 1);
  const dot = name.lastIndexOf('.');
  return { kind: 'file-type', ext: dot > 0 ? name.slice(dot + 1).toLowerCase() : null };
}

/** One sentence for the inspector. */
export function passthroughSentence(r: PassthroughReason): string {
  switch (r.kind) {
    case 'compression-off':
      return 'Stored whole: compression is off for this bucket.';
    case 'file-type':
      return r.ext
        ? `Stored whole: this bucket does not try deltas for .${r.ext} files.`
        : 'Stored whole: this bucket tries deltas only for files with a known extension.';
    case 'delta-not-kept':
      return 'Stored whole: the delta against this folder’s baseline was not small enough to keep.';
    case 'unknown':
      return 'Stored whole, without a delta.';
  }
}
