import { useState, useEffect } from 'react';
import { getPrefixSavings } from '../adminApi';
import { type DeltaSummary, summaryFromResponse } from '../deltaSummary';

interface SummaryOptions {
  bucket: string;
  prefix: string;
  active: boolean;
  connected: boolean;
  /** The endpoint is admin-only: without an admin session, send nothing. */
  adminSession: boolean;
  /** Bumped after a user's change: the walk runs again. */
  trigger: number;
}

/**
 * Per-prefix delta savings, fetched from the server-side endpoint that owns
 * the canonical (reference-aware) math. A client-side accumulator over the
 * listing undercounted by one `reference.bin` per deltaspace and could read
 * "100% saved" for ROR-shaped buckets. See `src/api/admin/savings.rs` for
 * the wire shape.
 */
export function useDeltaSummary({ bucket, prefix, active, connected, adminSession, trigger }: SummaryOptions) {
  const [deltaSummary, setDeltaSummary] = useState<DeltaSummary | null>(null);
  useEffect(() => {
    if (!connected || !active) return;
    // Use the URL-derived `bucket` (not the module-level getBucket()) so the
    // fetched savings always match the bucket the URL is showing. Reading global
    // state here raced the setBucket() sync effect: switching bucket A→B without
    // changing prefix would fetch A's savings and commit them under B's view.
    if (!bucket) return;
    if (!adminSession) {
      setDeltaSummary(null);
      return;
    }
    // Leaving the folder aborts the request: a walk the user no longer
    // looks at must not keep running beside the next folder's walk.
    const request = new AbortController();
    setDeltaSummary((prev) => (prev ? { ...prev, loading: true } : null));
    getPrefixSavings(bucket, prefix, request.signal)
      .then((resp) => {
        if (request.signal.aborted) return;
        if (!resp) {
          // Admin endpoint refused (no admin session) — keep the chip
          // hidden rather than guessing client-side.
          setDeltaSummary(null);
          return;
        }
        setDeltaSummary(summaryFromResponse(resp));
      })
      .catch(() => {
        if (request.signal.aborted) return;
        setDeltaSummary(null);
      });
    return () => request.abort();
  }, [connected, active, bucket, prefix, trigger, adminSession]);
  return deltaSummary;
}
