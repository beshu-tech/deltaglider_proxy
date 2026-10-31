import { Button } from 'antd';
import { ClockCircleOutlined, PlayCircleOutlined, StopOutlined, ReloadOutlined } from '@ant-design/icons';
import { useColors } from '../ThemeContext';
import { formatBytes, relativeTime, noun } from '../utils';
import { fmtNum } from './dashboard/chartDefaults';
import type { BucketScanProgress } from '../adminApi';
import { scanCallToAction } from '../scanQueue';

interface ScanStatusBannerProps {
  colors: ReturnType<typeof useColors>;
  liveProgress: BucketScanProgress | null;
  queue: string[];
  /** Count of buckets that have a cached scan (subset of bucketCount). */
  cachedCount: number;
  /** Total bucket roster size. */
  bucketCount: number;
  newestCompletedAt: string | null;
  oldestCompletedAt: string | null;
  unscannedCount: number;
  /** Buckets whose cached scan is older than the 6h freshness window. */
  staleCount: number;
  allBucketsCount: number;
  scansLoaded: boolean;
  isScanning: boolean;
  onStop: () => void;
  onScanMissing: () => void;
  onRescanAll: () => void;
}

/**
 * Scan-status banner. Lives ABOVE the grid so a "Re-scan in
 * progress" pulse doesn't shake the panel heights. The banner is the
 * contract with the user:
 *   - On first paint they see "Cache: N of M buckets · oldest
 *     scanned X ago". No ghosting, no spinner.
 *   - Click a scan action → banner switches to a live progress row with
 *     bucket name, objects scanned, bytes seen, Stop.
 *   - The KPIs and panels below update LIVE per SSE frame so you can
 *     watch the totals tick up rather than wait for the whole bucket
 *     to finish.
 */
export default function ScanStatusBanner({
  colors,
  liveProgress,
  queue,
  cachedCount,
  bucketCount,
  newestCompletedAt,
  oldestCompletedAt,
  unscannedCount,
  staleCount,
  allBucketsCount,
  scansLoaded,
  isScanning,
  onStop,
  onScanMissing,
  onRescanAll,
}: ScanStatusBannerProps) {
  return (
    <div
      style={{
        display: 'flex',
        alignItems: 'center',
        gap: 12,
        padding: '12px 16px',
        marginBottom: 12,
        background: colors.BG_CARD,
        border: `1px solid ${colors.BORDER}`,
        borderRadius: 8,
      }}
    >
      <ClockCircleOutlined style={{ color: colors.TEXT_MUTED, fontSize: 14 }} />
      <div style={{ fontSize: 12, color: colors.TEXT_SECONDARY, flex: 1, minWidth: 0 }}>
        {liveProgress ? (
          <>
            <span style={{ color: colors.ACCENT_BLUE, fontWeight: 700 }}>
              Scanning {liveProgress.bucket}
            </span>{' '}
            · {fmtNum(liveProgress.objects)} {noun(liveProgress.objects, 'object')} ·{' '}
            {formatBytes(liveProgress.original_bytes)} seen ·{' '}
            {liveProgress.pages_done} pages
            {queue.length > 1 && (
              <span style={{ color: colors.TEXT_MUTED }}>
                {' '}· {queue.length - 1} more queued
              </span>
            )}
          </>
        ) : cachedCount === 0 ? (
          <span>
            No bucket is scanned. Click <strong>{scanCallToAction(unscannedCount, bucketCount)}</strong>. The results are kept on disk and survive a restart.
          </span>
        ) : (
          <>
            <strong>{cachedCount}</strong> of{' '}
            <strong>{bucketCount}</strong> buckets scanned · Newest:{' '}
            <span style={{ color: colors.TEXT_PRIMARY }}>{relativeTime(newestCompletedAt)}</span>
            {oldestCompletedAt && oldestCompletedAt !== newestCompletedAt && (
              <>
                {' '}· Oldest:{' '}
                <span style={{ color: colors.TEXT_PRIMARY }}>{relativeTime(oldestCompletedAt)}</span>
              </>
            )}
            {unscannedCount > 0 && (
              <span style={{ color: colors.ACCENT_AMBER }}>
                {' '}· {unscannedCount} unscanned excluded from totals
              </span>
            )}
            {staleCount > 0 && (
              <span style={{ color: colors.ACCENT_AMBER }} title="Cached results older than 6 hours. Data may have drifted — re-scan for fresh numbers.">
                {' '}· <strong>{staleCount}</strong> stale (&gt;6h) — re-scan for fresh numbers
              </span>
            )}
          </>
        )}
      </div>
      {isScanning ? (
        <Button
          size="small"
          danger
          icon={<StopOutlined />}
          onClick={onStop}
        >
          Stop
        </Button>
      ) : (
        /*
          The labels of the analytics page: "Scan all buckets" / "Scan N
          more buckets" while some bucket has no scan (a full re-scan then
          redoes buckets whose numbers are known), and "Re-scan all" only
          once a scan exists.
        */
        <div style={{ display: 'flex', gap: 8 }}>
          {unscannedCount > 0 && (
            <Button
              size="small"
              type="primary"
              icon={<PlayCircleOutlined />}
              onClick={onScanMissing}
              disabled={allBucketsCount === 0 || !scansLoaded}
            >
              {scanCallToAction(unscannedCount, bucketCount)}
            </Button>
          )}
          {cachedCount > 0 && (
            <Button
              size="small"
              type={unscannedCount === 0 ? 'primary' : 'default'}
              icon={<ReloadOutlined />}
              onClick={onRescanAll}
              disabled={allBucketsCount === 0 || !scansLoaded}
              title="Scan every bucket again"
            >
              Re-scan all
            </Button>
          )}
        </div>
      )}
    </div>
  );
}
