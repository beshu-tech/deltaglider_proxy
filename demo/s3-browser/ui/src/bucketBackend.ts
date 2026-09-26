/**
 * Backend chip rules for the browser (sidebar rows and breadcrumb).
 * React-free so src/__tests__/bucketBackend.test.ts can check it.
 *
 * The chip shows the REAL backend name from the admin bucket-origins data.
 * It never guesses a provider from a name or an endpoint (the old regex made
 * "HZ" red and "LOC" teal), and it is not colour-coded: red and amber are
 * for real problems only.
 */
import type { BucketBackendOrigin } from './types';

/** The label for a bucket's backend chip, or null when the origin data is
 *  not available (for example a non-admin session). */
export function backendChipLabel(origin: BucketBackendOrigin | undefined): string | null {
  const name = origin?.backendName?.trim();
  return name ? name : null;
}

/** Chips help only when the buckets sit on more than one backend. With one
 *  backend every row would carry the same chip, which is noise. */
export function showBackendChips(origins: (BucketBackendOrigin | undefined)[]): boolean {
  const names = new Set<string>();
  for (const o of origins) {
    const label = backendChipLabel(o);
    if (label) names.add(label);
  }
  return names.size > 1;
}

/** Hover text: every known fact about where the bucket lives. */
export function describeBackend(origin: BucketBackendOrigin | undefined): string {
  if (!origin) return '';
  return [
    origin.backendName ? `Backend: ${origin.backendName}` : null,
    origin.backendType ? `Type: ${origin.backendType}` : null,
    origin.backendEndpoint ? `Endpoint: ${origin.backendEndpoint}` : null,
    origin.backendRegion ? `Region: ${origin.backendRegion}` : null,
    origin.backendPath ? `Path: ${origin.backendPath}` : null,
    origin.realBucket ? `Real bucket: ${origin.realBucket}` : null,
  ].filter(Boolean).join('\n');
}
