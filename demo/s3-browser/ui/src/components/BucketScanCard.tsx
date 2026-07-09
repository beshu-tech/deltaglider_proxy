/**
 * BucketScanCard — the dashboard's "Objects stored" + "Storage
 * savings" headline.
 *
 * Replaces the old `/_/stats` card that capped its scan at 1,000
 * objects. This one backs onto the persistent bucket-scan engine in
 * `src/api/admin/bucket_scan.rs`:
 *
 *   • On mount, fetches every per-bucket scan result the server has
 *     cached on disk (no TTL — S3 data is write-mostly, so the cache
 *     is fine until the user explicitly re-scans).
 *   • Aggregates trustworthy totals across scanned buckets and shows
 *     the oldest scan's age so the user knows how stale the picture
 *     is.
 *   • Live scan: opens an SSE stream against `/scan/stream?bucket=X`
 *     and replaces the headline with a progress bar + ETA. Subscription
 *     auto-closes on the "done" frame.
 *   • Cancel and forget controls are right there.
 *   • "All buckets" mode fans the scan out across every bucket
 *     sequentially in the background — closing the tab is fine, the
 *     server keeps the jobs running and persists each one as it
 *     finishes.
 *
 * Three visual states:
 *   - **idle**: no scans on disk → big "Run scan" CTA.
 *   - **running**: SSE feed live → progress bar + counts ticking up.
 *   - **done**: at least one scan result present → totals + "scanned
 *     N ago" line + "Re-scan" button.
 */

import { useState, useEffect, useCallback, useRef } from 'react';
import { Button, Progress, Tooltip } from 'antd';
import {
  PlayCircleOutlined,
  StopOutlined,
  ReloadOutlined,
  ClockCircleOutlined,
} from '@ant-design/icons';
import { useColors } from '../ThemeContext';
import { formatBytes } from '../utils';
import { summarizeScopeSavings } from '../savings';
import { fmtNum } from './dashboard/chartDefaults';
import {
  getAllBucketScans,
  startBucketScan,
  stopBucketScan,
  subscribeBucketScan,
  type BucketScanResult,
  type BucketScanProgress,
} from '../adminApi';
import { listBuckets } from '../s3client';

interface Props {
  /**
   * Render slot for a Panel — we own the body, the caller wraps it in
   * a Panel so the visual sizing stays consistent with the rest of
   * the dashboard.
   */
  children?: never;
  /**
   * The card needs to influence its surrounding Panel's `actions`
   * slot (the Scan / Stop / Forget buttons live in the header). This
   * lets the parent inject the rendered buttons into the panel
   * header.
   */
  onRenderActions?: (actions: React.ReactNode) => void;
  /**
   * Optional override — if set, the card scans this single bucket
   * only. When null/undefined, the card aggregates across all
   * buckets.
   */
  scopeBucket?: string | null;
}

/** Format a duration like "3h 21m ago" or "47s ago". */
function ageLabel(iso: string): string {
  const ms = Date.now() - new Date(iso).getTime();
  if (ms < 0) return 'just now';
  const s = Math.floor(ms / 1000);
  if (s < 60) return `${s}s ago`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ago`;
  const h = Math.floor(m / 60);
  if (h < 24) {
    const mm = m % 60;
    return mm ? `${h}h ${mm}m ago` : `${h}h ago`;
  }
  const d = Math.floor(h / 24);
  return `${d}d ago`;
}

/** Aggregate a map of per-bucket results into one synthetic total. */
function aggregate(results: Record<string, BucketScanResult>): {
  buckets: number;
  objects: number;
  originalBytes: number;
  storedBytes: number;
  savings: number;
  oldestCompletedAt: string | null;
  newestCompletedAt: string | null;
} {
  let objects = 0;
  let originalBytes = 0;
  let storedBytes = 0;
  let oldest: string | null = null;
  let newest: string | null = null;
  let count = 0;
  for (const r of Object.values(results)) {
    count += 1;
    objects += r.total_objects;
    originalBytes += r.total_original_bytes;
    storedBytes += r.total_stored_bytes;
    if (!oldest || r.completed_at < oldest) oldest = r.completed_at;
    if (!newest || r.completed_at > newest) newest = r.completed_at;
  }
  // Route through the canonical scope-savings helper — same cap and
  // clamp behaviour as the chip, the delta_efficiency report, and
  // every other surface. Pre-consolidation this had its own
  // uncapped/unclamped formula, which could read `99.95%` for the
  // exact same data the chip displayed as `99%`.
  const savings = summarizeScopeSavings(originalBytes, storedBytes).pct;
  return {
    buckets: count,
    objects,
    originalBytes,
    storedBytes,
    savings,
    oldestCompletedAt: oldest,
    newestCompletedAt: newest,
  };
}

export default function BucketScanCard({ onRenderActions, scopeBucket }: Props) {
  const colors = useColors();

  const [scans, setScans] = useState<Record<string, BucketScanResult>>({});
  const [scansLoaded, setScansLoaded] = useState(false);
  const [liveProgress, setLiveProgress] = useState<BucketScanProgress | null>(
    null,
  );
  /**
   * Multi-bucket fan-out queue. When the user runs an "all buckets"
   * scan, we walk the bucket list sequentially. The current head of
   * the queue is whatever bucket the live SSE is following.
   */
  const [queue, setQueue] = useState<string[]>([]);
  const [allBuckets, setAllBuckets] = useState<string[]>([]);
  const unsubRef = useRef<(() => void) | null>(null);

  /** Hard-refresh the all-bucket cache (cheap — server-side disk read). */
  const refreshAllScans = useCallback(async () => {
    try {
      const res = await getAllBucketScans();
      setScans(res.buckets);
    } catch {
      // Non-fatal — leave whatever's already there.
    } finally {
      setScansLoaded(true);
    }
  }, []);

  // Initial load: scans + bucket list (for the fan-out).
  useEffect(() => {
    refreshAllScans();
    listBuckets()
      .then((bs) => setAllBuckets(bs.filter((b) => !b.unavailable).map((b) => b.name)))
      .catch(() => setAllBuckets([]));
    // Re-poll every 30s — covers the case where another tab kicked
    // off a scan that completed in the background. SSE handles the
    // live case for the tab that started a scan; this catches
    // cross-tab settling.
    const id = window.setInterval(refreshAllScans, 30_000);
    return () => window.clearInterval(id);
  }, [refreshAllScans]);

  /** Tear down any live subscription on unmount. */
  useEffect(() => {
    return () => {
      if (unsubRef.current) unsubRef.current();
      unsubRef.current = null;
    };
  }, []);

  /** Wire an SSE subscription for a single bucket. */
  const subscribe = useCallback(
    (bucket: string) => {
      if (unsubRef.current) unsubRef.current();
      unsubRef.current = subscribeBucketScan(
        bucket,
        (frame) => {
          setLiveProgress(frame);
          if (frame.finished) {
            // Re-pull the cache so the headline switches to the
            // newly-completed result, then advance the queue.
            refreshAllScans();
            setLiveProgress(null);
            setQueue((q) => q.slice(1));
          }
        },
        () => {
          // Transport error: tear down so the UI doesn't show a
          // ghost progress bar forever.
          setLiveProgress(null);
          if (unsubRef.current) unsubRef.current();
          unsubRef.current = null;
        },
      );
    },
    [refreshAllScans],
  );

  /** When the queue head changes, follow that bucket. */
  useEffect(() => {
    if (queue.length === 0) {
      if (unsubRef.current) {
        unsubRef.current();
        unsubRef.current = null;
      }
      return;
    }
    const head = queue[0];
    // Kick the scan (idempotent — if it's already running we just
    // attach to it) then subscribe.
    startBucketScan(head).catch(() => {
      // If start fails, skip this bucket and advance.
      setQueue((q) => q.slice(1));
    });
    subscribe(head);
  }, [queue, subscribe]);

  // ─── Actions ──────────────────────────────────────────────────────

  const handleScanOne = useCallback(
    (bucket: string) => {
      setQueue([bucket]);
    },
    [],
  );

  const handleScanAll = useCallback(() => {
    if (allBuckets.length === 0) return;
    setQueue(allBuckets);
  }, [allBuckets]);

  const handleStop = useCallback(() => {
    const head = queue[0];
    if (head) {
      stopBucketScan(head).catch(() => {});
    }
    setQueue([]);
    setLiveProgress(null);
  }, [queue]);

  // ─── Render ───────────────────────────────────────────────────────

  // Project actions back up so the Panel header can hold them.
  useEffect(() => {
    if (!onRenderActions) return;
    if (liveProgress) {
      onRenderActions(
        <Button
          size="small"
          danger
          icon={<StopOutlined />}
          onClick={handleStop}
        >
          Stop
        </Button>,
      );
    } else if (scopeBucket) {
      onRenderActions(
        <Button
          size="small"
          icon={<ReloadOutlined />}
          onClick={() => handleScanOne(scopeBucket)}
        >
          {scans[scopeBucket] ? 'Re-scan' : 'Scan'}
        </Button>,
      );
    } else {
      onRenderActions(
        <Button
          size="small"
          type={Object.keys(scans).length === 0 ? 'primary' : 'default'}
          icon={
            Object.keys(scans).length === 0 ? (
              <PlayCircleOutlined />
            ) : (
              <ReloadOutlined />
            )
          }
          onClick={handleScanAll}
          disabled={allBuckets.length === 0}
        >
          {Object.keys(scans).length === 0
            ? 'Run full scan'
            : `Re-scan all (${allBuckets.length})`}
        </Button>,
      );
    }
  }, [
    liveProgress,
    scopeBucket,
    scans,
    allBuckets.length,
    handleScanOne,
    handleScanAll,
    handleStop,
    onRenderActions,
  ]);

  // 1. Running state — show live progress.
  if (liveProgress) {
    const headBucket = queue[0];
    const queuedRemaining = Math.max(0, queue.length - 1);
    // We don't know total objects in advance, so the progress bar
    // animates a soft "pages done" indicator without a denominator.
    // The truthful counters are right next to it.
    return (
      <div style={{ display: 'flex', flexDirection: 'column', height: '100%' }}>
        <div
          style={{
            fontSize: 11,
            fontWeight: 700,
            letterSpacing: '0.06em',
            textTransform: 'uppercase',
            color: colors.TEXT_MUTED,
            marginBottom: 4,
          }}
        >
          Scanning {headBucket}
          {queuedRemaining > 0 ? ` (${queuedRemaining} queued)` : ''}
        </div>
        <div
          style={{
            fontSize: 28,
            fontWeight: 700,
            color: colors.TEXT_PRIMARY,
            lineHeight: 1.2,
          }}
        >
          {fmtNum(liveProgress.objects)}{' '}
          <span style={{ fontSize: 14, fontWeight: 500, color: colors.TEXT_SECONDARY }}>
            objects scanned
          </span>
        </div>
        <div
          style={{
            fontSize: 12,
            color: colors.TEXT_SECONDARY,
            marginTop: 4,
            marginBottom: 10,
          }}
        >
          {formatBytes(liveProgress.original_bytes)} so far ·{' '}
          {liveProgress.original_bytes > 0
            ? `${summarizeScopeSavings(liveProgress.original_bytes, liveProgress.stored_bytes).pctOneDecimal}% savings so far`
            : 'computing savings…'}
        </div>
        <Progress
          percent={Math.min(99, liveProgress.pages_done * 1.5)}
          showInfo={false}
          status="active"
          strokeColor={colors.ACCENT_BLUE}
        />
        <div
          style={{
            marginTop: 'auto',
            fontSize: 11,
            color: colors.TEXT_MUTED,
          }}
        >
          {liveProgress.pages_done} pages · {liveProgress.has_more ? 'more pages remaining' : 'finalising…'}
        </div>
      </div>
    );
  }

  // 2. Done state — at least one bucket has a cached result.
  const scopeResult = scopeBucket ? scans[scopeBucket] : null;
  const hasAnyScan = scopeBucket ? !!scopeResult : Object.keys(scans).length > 0;

  if (hasAnyScan) {
    const totals = scopeResult
      ? {
          buckets: 1,
          objects: scopeResult.total_objects,
          originalBytes: scopeResult.total_original_bytes,
          storedBytes: scopeResult.total_stored_bytes,
          savings: scopeResult.savings_percentage,
          oldestCompletedAt: scopeResult.completed_at,
          newestCompletedAt: scopeResult.completed_at,
        }
      : aggregate(scans);

    const totalBuckets = scopeBucket ? 1 : allBuckets.length;
    const unscanned = scopeBucket
      ? 0
      : Math.max(0, totalBuckets - totals.buckets);

    return (
      <div style={{ display: 'flex', flexDirection: 'column', height: '100%' }}>
        <div
          style={{
            fontSize: 36,
            fontWeight: 700,
            color: colors.TEXT_PRIMARY,
            lineHeight: 1.1,
          }}
        >
          {fmtNum(totals.objects)}{' '}
          <span style={{ fontSize: 14, fontWeight: 500, color: colors.TEXT_SECONDARY }}>
            objects
          </span>
        </div>
        <div
          style={{
            fontSize: 14,
            color: colors.TEXT_SECONDARY,
            marginTop: 4,
          }}
        >
          {formatBytes(totals.originalBytes)} original ·{' '}
          <span style={{ color: totals.savings > 10 ? colors.ACCENT_GREEN : colors.TEXT_SECONDARY, fontWeight: 600 }}>
            {totals.savings.toFixed(1)}% saved
          </span>{' '}
          ({formatBytes(totals.originalBytes - totals.storedBytes)})
        </div>

        {/* Scan-age line — the user MUST see this so they don't trust
            stale numbers without realising it. */}
        <div
          style={{
            marginTop: 10,
            display: 'flex',
            alignItems: 'center',
            gap: 6,
            fontSize: 11,
            color: colors.TEXT_MUTED,
          }}
        >
          <ClockCircleOutlined style={{ fontSize: 10 }} />
          {scopeBucket ? (
            <span>Scanned {ageLabel(totals.newestCompletedAt!)}</span>
          ) : (
            <span>
              Newest: {ageLabel(totals.newestCompletedAt!)} ·{' '}
              Oldest: {ageLabel(totals.oldestCompletedAt!)}
            </span>
          )}
        </div>

        {unscanned > 0 && (
          <Tooltip title="Click Re-scan all to include them in the totals.">
            <div
              style={{
                marginTop: 4,
                fontSize: 11,
                color: colors.ACCENT_AMBER,
              }}
            >
              {unscanned} of {totalBuckets} buckets not measured yet — press "Scan all" to include them
            </div>
          </Tooltip>
        )}

        {scansLoaded && (
          <div
            style={{
              marginTop: 'auto',
              fontSize: 11,
              color: colors.TEXT_MUTED,
            }}
          >
            Updated at last scan
          </div>
        )}
      </div>
    );
  }

  // 3. Idle state — nothing scanned yet. Big honest CTA.
  return (
    <div
      style={{
        display: 'flex',
        flexDirection: 'column',
        justifyContent: 'center',
        alignItems: 'flex-start',
        gap: 10,
        height: '100%',
      }}
    >
      <div
        style={{
          fontSize: 14,
          color: colors.TEXT_SECONDARY,
          lineHeight: 1.5,
        }}
      >
        No scans yet.
        <br />
        Click <strong>Run full scan</strong> to walk every bucket and
        compute honest totals. Safe to run in the background — results
        persist across restarts.
      </div>
      <div style={{ fontSize: 11, color: colors.TEXT_MUTED }}>
        {allBuckets.length} bucket{allBuckets.length === 1 ? '' : 's'} ready to scan
      </div>
    </div>
  );
}
