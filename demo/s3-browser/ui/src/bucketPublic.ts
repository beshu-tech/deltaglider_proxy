import type { AdminConfig } from './adminApi';
import { bucketPolicyFor } from './bucketPolicyLookup';

/** How much of a bucket anonymous clients can read: the whole bucket, some
 *  prefixes, or nothing (`null`). */
export type PublicScope = { whole: true } | { whole: false; prefixes: string[] };

/**
 * The public scope of `bucket` from the admin config. `public: true` and a
 * `""` prefix both mean the whole bucket (the server expands one into the
 * other). Unknown config (no admin session) = `null`: never a false marker.
 */
export function publicScope(
  config: Pick<AdminConfig, 'bucket_policies'> | null | undefined,
  bucket: string,
): PublicScope | null {
  const policy = bucketPolicyFor(config, bucket);
  if (!policy) return null;
  const prefixes = policy.public_prefixes ?? [];
  if (policy.public || prefixes.includes('')) return { whole: true };
  return prefixes.length > 0 ? { whole: false, prefixes } : null;
}

/** The hover text of the public marker. */
export function publicScopeText(scope: PublicScope): string {
  return scope.whole
    ? 'Public: anyone can read every object in this bucket without credentials.'
    : `Partly public: anyone can read objects under ${scope.prefixes.join(', ')} without credentials.`;
}

/**
 * The header marker for the folder `prefix` of a bucket with `scope`: the
 * bucket-level chip described the bucket in every folder, so a private
 * folder read "Partly public" and the public folder itself under-stated it.
 * `null` = nothing anonymous clients can read here.
 */
export function publicMarkerAt(scope: PublicScope, prefix: string): { label: string; text: string } | null {
  if (scope.whole) return { label: 'Public', text: publicScopeText(scope) };
  if (!prefix) return { label: 'Partly public', text: publicScopeText(scope) };
  const covering = scope.prefixes.find((p) => prefix.startsWith(p));
  if (covering !== undefined) {
    return {
      label: 'Public folder',
      text: `Public folder: anyone can read every object under ${covering} without credentials.`,
    };
  }
  const inside = scope.prefixes.filter((p) => p.startsWith(prefix));
  if (inside.length > 0) {
    return {
      label: 'Partly public',
      text: `Partly public: anyone can read objects under ${inside.join(', ')} without credentials.`,
    };
  }
  return null;
}
