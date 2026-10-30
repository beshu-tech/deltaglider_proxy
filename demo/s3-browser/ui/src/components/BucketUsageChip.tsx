import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query';
import { ReloadOutlined } from '@ant-design/icons';
import { getBucketUsage, refreshBucketUsage } from '../adminApi';
import { useBucketOrigins } from '../queries/backends';
import { qk } from '../queries/keys';
import { useColors } from '../ThemeContext';
import { formatBytes, formatBytesIec, noun } from '../utils';
import { useAdminConfig } from '../queries/config';
import { bucketPolicyFor } from '../bucketPolicyLookup';
import { relativeTime } from '../utils';

/**
 * O(1) bucket-size pill in the TopBar — the Ceph-style running counter
 * (`src/bucket_usage.rs`): `<size> · <N> objects`, maintained inline on every
 * PUT/DELETE so it's instant even on huge buckets. The ⟳ button forces an
 * authoritative full scan (the only O(n) path). Auto-hides without an admin
 * session (the endpoint 403s → null). `last_scan_at` shows when a scan last
 * reconciled the running total; only a scan that no write overlapped sets it.
 */
export default function BucketUsageChip({
  bucket,
  canAdmin,
  inFolder = false,
}: {
  bucket: string;
  canAdmin: boolean;
  /** True when the breadcrumb shows a folder: the pill sits right after it
   *  but counts the WHOLE bucket, so it says so instead of reading as the
   *  folder's own size. */
  inFolder?: boolean;
}) {
  const c = useColors();
  const qc = useQueryClient();

  const { data } = useQuery({
    queryKey: qk.bucketUsage(bucket),
    queryFn: () => getBucketUsage(bucket),
    enabled: canAdmin && !!bucket,
    staleTime: 15_000,
  });

  const refresh = useMutation({
    mutationFn: () => refreshBucketUsage(bucket),
    onSuccess: (row) => {
      if (row) qc.setQueryData(qk.bucketUsage(bucket), row);
    },
  });

  // When the bucket's backend is unreachable, the running counter is
  // UNVERIFIABLE — showing the stale number as fact misled the beshu-b2
  // incident (21,317 objects displayed for a bucket the proxy couldn't even
  // reach). Render a warning chip instead. Shares the cached origins query.
  const origins = useBucketOrigins({ enabled: canAdmin && !!bucket });
  const unavailable = origins.data?.buckets.find((b) => b.name === bucket)?.unavailable;
  // The quota, when the bucket has one: the server's gate compares it with
  // the STORED bytes, so the chip shows those against it.
  const { data: config } = useAdminConfig({ enabled: canAdmin && !!bucket });
  const quota = bucketPolicyFor(config, bucket)?.quota_bytes ?? null;

  // No session, no data, or a "disabled" payload (open-mode dev) → render nothing.
  if (!canAdmin || !data || data.object_count == null) return null;

  if (unavailable) {
    return (
      <span
        title={`Backend unreachable — the stored size/count can't be verified. ${unavailable}`}
        style={{
          display: 'inline-flex',
          alignItems: 'center',
          gap: 6,
          padding: '2px 8px',
          borderRadius: 6,
          border: `1px solid ${c.ACCENT_AMBER}66`,
          background: c.BG_ELEVATED,
          fontSize: 12,
          lineHeight: '18px',
          color: c.ACCENT_AMBER,
          whiteSpace: 'nowrap',
          cursor: 'default',
        }}
      >
        <span style={{ width: 7, height: 7, borderRadius: '50%', background: c.ACCENT_AMBER }} />
        Backend unreachable
      </span>
    );
  }

  // The last Refresh of this chip could not read the size of some objects:
  // the server kept the counter and answered the scan's own numbers apart.
  const estimate = refresh.data?.estimated ? refresh.data.estimate : undefined;
  const scope = `Whole bucket "${bucket}" (all folders). `;
  const scannedTitle =
    scope +
    (data.last_scan_at != null
      ? `Last full scan ${relativeTime(data.last_scan_at * 1000)}`
      : 'Not reconciled — the running total of writes and deletes may be off (never scanned, the last scan ran while writes changed the bucket, or a write could not read the object it replaced); ⟳ to reconcile when no writes are running') +
    (refresh.data?.estimated
      ? '. The last Refresh could not read the size of some objects, so it kept the running total' +
        (estimate
          ? `; the scan estimated about ${formatBytes(estimate.logical_bytes)} in ${estimate.object_count.toLocaleString()} ${noun(estimate.object_count, 'object')}`
          : '')
      : '');

  // Quota: share of it used, amber from 90 %. A quota of 0 freezes the bucket.
  const quotaShare = quota != null && quota > 0 ? data.stored_bytes / quota : null;
  const quotaTone = quota === 0 || (quotaShare != null && quotaShare >= 0.9) ? c.ACCENT_AMBER : c.TEXT_MUTED;
  const quotaTitle = quota == null
    ? ''
    : quota === 0
      ? ' Quota 0: the bucket takes no new writes.'
      : ` Quota: ${formatBytes(data.stored_bytes)} stored of ${formatBytesIec(quota)} (writes that pass the quota are refused).`;

  // Plain text after the breadcrumb, not a box: a framed pill the height of
  // the header read as a second breadcrumb tab. The Header's 52px line
  // height is reset so the row stays one line of text.
  return (
    <span
      title={scannedTitle + quotaTitle}
      style={{
        display: 'inline-flex',
        alignItems: 'center',
        gap: 6,
        fontSize: 12,
        lineHeight: '18px',
        color: c.TEXT_SECONDARY,
        whiteSpace: 'nowrap',
        cursor: 'default',
        fontFamily: 'var(--font-ui)',
      }}
    >
      {inFolder && <span style={{ color: c.TEXT_MUTED }}>Bucket total</span>}
      {refresh.data?.estimated && (
        <span style={{ color: c.ACCENT_AMBER }} aria-label="estimated">
          ≈
        </span>
      )}
      <strong style={{ color: c.TEXT_PRIMARY, fontVariantNumeric: 'tabular-nums' }}>
        {formatBytes(data.logical_bytes)}
      </strong>
      <span style={{ color: c.TEXT_MUTED }}>·</span>
      <span style={{ fontVariantNumeric: 'tabular-nums' }}>
        {data.object_count.toLocaleString()} {noun(data.object_count, 'object')}
      </span>
      {quota != null && (
        <>
          <span style={{ color: c.TEXT_MUTED }}>·</span>
          <span data-testid="bucket-quota" style={{ color: quotaTone, fontVariantNumeric: 'tabular-nums' }}>
            {quota === 0
              ? 'Frozen (quota 0)'
              : `${Math.min(999, Math.round((quotaShare ?? 0) * 100))}% of ${formatBytesIec(quota)} quota`}
          </span>
        </>
      )}
      <button
        type="button"
        onClick={() => !refresh.isPending && refresh.mutate()}
        disabled={refresh.isPending}
        title="Refresh (full scan)"
        aria-label={`Refresh the size of bucket ${bucket} (full scan)`}
        style={{
          display: 'inline-flex', padding: 0, border: 'none', background: 'none',
          cursor: refresh.isPending ? 'default' : 'pointer',
          color: c.ACCENT_BLUE,
          fontSize: 11,
        }}
      >
        <ReloadOutlined spin={refresh.isPending} aria-hidden="true" />
      </button>
    </span>
  );
}
