import { Button } from 'antd';
import { ClockCircleOutlined, PlayCircleOutlined, StopOutlined, ReloadOutlined } from '@ant-design/icons';
import { useColors } from '../ThemeContext';
import { formatBytes, ageLabel } from '../utils';
import { fmtNum } from './dashboard/chartDefaults';
import type { BucketScanProgress } from '../adminApi';

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
 *   - Click Re-scan all → banner switches to a live progress row with
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
        padding: '10px 14px',
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
            · {fmtNum(liveProgress.objects)} objects ·{' '}
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
            No scans yet. Click <strong>Run full scan</strong> — results persist on disk and survive restarts.
          </span>
        ) : (
          <>
            <strong>{cachedCount}</strong> of{' '}
            <strong>{bucketCount}</strong> buckets scanned · Newest:{' '}
            <span style={{ color: colors.TEXT_PRIMARY }}>{ageLabel(newestCompletedAt)}</span>
            {oldestCompletedAt && oldestCompletedAt !== newestCompletedAt && (
              <>
                {' '}· Oldest:{' '}
                <span style={{ color: colors.TEXT_PRIMARY }}>{ageLabel(oldestCompletedAt)}</span>
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
          Two affordances side-by-side:
          - **Scan missing (N)** is the primary action when any
            bucket is uncached. Doing a full re-scan in that case
            would waste 30+ minutes redoing the 1.4 TB beshu bucket
            just to learn the same number we already have.
          - **Re-scan all** is the explicit "I want fresh numbers
            everywhere" — surfaced as a smaller secondary button.
          When everything is already cached, "Scan missing" hides
          and only "Re-scan all" remains as the primary.
        */
        <div style={{ display: 'flex', gap: 6 }}>
          {unscannedCount > 0 && (
            <Button
              size="small"
              type="primary"
              icon={<PlayCircleOutlined />}
              onClick={onScanMissing}
              disabled={allBucketsCount === 0 || !scansLoaded}
            >
              Scan missing ({unscannedCount})
            </Button>
          )}
          <Button
            size="small"
            type={unscannedCount === 0 ? 'primary' : 'default'}
            icon={
              cachedCount === 0 ? (
                <PlayCircleOutlined />
              ) : (
                <ReloadOutlined />
              )
            }
            onClick={onRescanAll}
            disabled={allBucketsCount === 0 || !scansLoaded}
            title={
              unscannedCount > 0
                ? 'Force a fresh scan of every bucket, including ones already cached. Expensive — only do this if you suspect data drift.'
                : undefined
            }
          >
            {cachedCount === 0
              ? 'Run full scan'
              : `Re-scan all (${allBucketsCount})`}
          </Button>
        </div>
      )}
    </div>
  );
}
