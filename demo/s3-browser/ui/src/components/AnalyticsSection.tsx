/**
 * AnalyticsSection — cost + storage savings view.
 *
 * Shares the DashboardGrid / Panel primitives with MetricsPage so
 * both tabs read as one tool. Four rows:
 *   - Scan-status banner: cache age + Re-scan-all / Stop controls.
 *   - KPI strip: Total Storage · Space Saved · Savings % · Est. Monthly Savings.
 *   - Storage by bucket horizontal bar + Top-5 table.
 *   - Session savings time-series + Compression opportunities.
 *
 * Data path (post v0.10):
 *   - On mount, call `getAllBucketScans()` once. That returns every
 *     bucket the server has a cached scan for, loaded from
 *     `.deltaglider_scans/<bucket>.json` on disk — survives restarts.
 *     KPIs render IMMEDIATELY off this cache; no spinner unless
 *     truly cold.
 *   - In parallel, `listBuckets()` learns the FULL bucket roster so
 *     we know which buckets have NO cache yet ("unscanned").
 *   - The user can click **Re-scan all** to walk every bucket
 *     sequentially via SSE. Each bucket streams progress; on `done`
 *     we re-pull `getAllBucketScans()` so the panels reflect the
 *     newly-persisted totals. The next bucket in the queue starts.
 *   - **Stop** cancels the bucket that's currently streaming. The
 *     queue is dropped. Whatever was already persisted stays
 *     persisted — there is no "rollback".
 *
 * No TTL: S3 is write-mostly, the cache is fine until the user
 * explicitly re-scans. The banner shows oldest + newest scan age so
 * staleness is never invisible.
 */
import { useState, useEffect, useCallback, useRef, useMemo } from 'react';
import { Button } from 'antd';
import { CaretRightOutlined, ReloadOutlined } from '@ant-design/icons';
import { useColors } from '../ThemeContext';
import { useVisiblePolling } from '../useVisiblePolling';
import { listBuckets } from '../s3client';
import { getBucketUsage } from '../adminApi';
import type { AdminConfig, BucketUsage } from '../adminApi';
import {
  getAllBucketScans,
  startBucketScan,
  stopBucketScan,
  subscribeBucketScan,
  type BucketScanResult,
  type BucketScanProgress,
} from '../adminApi';
import { formatBytes, ageLabel } from '../utils';
import { summarizeScopeSavings } from '../savings';
import DashboardGrid from './dashboard/DashboardGrid';
import Panel from './dashboard/Panel';
import HeroSavingsPanel from './HeroSavingsPanel';
import TopBucketsSortSelect from './TopBucketsSortSelect';
import BucketFleetList from './BucketFleetList';
import ScanStatusBanner from './ScanStatusBanner';
import { isScanStale } from './scanFreshness';
import { SORT_LABELS, type TopBucketsSortKey } from './topBucketsSort';

/** Row shape consumed by chart + table. Derived from cache + live progress. */
export interface BucketRow {
  bucket: string;
  totalOriginal: number;
  totalStored: number;
  savings: number;
  savingsPercent: number;
  objectCount: number;
  /** ISO completion date — null when this bucket has no cached scan yet. */
  completedAt: string | null;
  /** True while this bucket's SSE feed is open. */
  scanning: boolean;
  /** Per-bucket live progress, only set while `scanning`. */
  progress?: BucketScanProgress;
}

interface Props {
  config: AdminConfig | null;
}

// COST_PRESETS now lives in HeroSavingsPanel.tsx (sole consumer).

export default function AnalyticsSection({ config }: Props) {
  const colors = useColors();

  /**
   * The two state slices that together define the analytics view:
   *   - `scans` is the disk-backed cache (server-persisted). Source
   *     of truth for "what totals do I display".
   *   - `liveProgress` is the SSE frame for whatever bucket is being
   *     scanned RIGHT NOW. Renders the per-row pulse and overrides
   *     that one bucket's totals until its scan completes.
   *   - `queue` is the list of buckets remaining in a "Re-scan all"
   *     fan-out. The head of the queue is what `liveProgress` is
   *     tracking; on `done` we shift it and start the next.
   */
  const [scans, setScans] = useState<Record<string, BucketScanResult>>({});
  const [scansLoaded, setScansLoaded] = useState(false);
  const [allBuckets, setAllBuckets] = useState<string[]>([]);
  // O(1) running counters, keyed by bucket. Seeds the fleet rows instantly on
  // load so the dashboard shows real sizes without waiting for a scan; a live
  // scan still overrides while it runs, and a completed cached scan takes
  // precedence (it's the same ground truth the counter is reconciled against).
  const [counters, setCounters] = useState<Record<string, BucketUsage>>({});
  const [liveProgress, setLiveProgress] = useState<BucketScanProgress | null>(
    null,
  );
  const [queue, setQueue] = useState<string[]>([]);
  /**
   * Top-buckets sort key. Persisted to localStorage so the operator's
   * preference survives reloads — repeatedly re-picking "savings" each
   * visit gets annoying.
   */
  const [topBucketsSort, setTopBucketsSort] = useState<TopBucketsSortKey>(() => {
    const saved = typeof localStorage !== 'undefined' && localStorage.getItem('dgp-top-sort');
    return (saved as TopBucketsSortKey) ?? 'original';
  });
  useEffect(() => {
    if (typeof localStorage !== 'undefined') localStorage.setItem('dgp-top-sort', topBucketsSort);
  }, [topBucketsSort]);
  const unsubRef = useRef<(() => void) | null>(null);

  const [costRate, setCostRate] = useState(() => {
    const saved = localStorage.getItem('dg-cost-per-gb');
    return saved ? parseFloat(saved) : 0.00524;
  });
  // The cost-rate cog popover (and the COST_PRESETS list, and the
  // localStorage setter) all live inside HeroSavingsPanel now —
  // AnalyticsSection just owns the rate value + the save callback so
  // the hero panel can swap presets.

  const saveCostRate = (rate: number) => {
    setCostRate(rate);
    localStorage.setItem('dg-cost-per-gb', String(rate));
  };

  /**
   * Pull the persisted scan map from the server. Idempotent; cheap.
   * Called on mount, on focus, every 30s (to catch cross-tab work),
   * and after each bucket's scan finishes so the panels reflect the
   * fresh per-bucket totals.
   */
  const refreshScans = useCallback(async () => {
    try {
      const res = await getAllBucketScans();
      setScans(res.buckets);
      return res.running ?? {};
    } catch {
      // Non-fatal: keep whatever's already on screen.
      return {};
    } finally {
      setScansLoaded(true);
    }
  }, []);

  // Initial load: bucket list + persisted scans, in parallel.
  //
  // RE-ATTACH on mount: scans run server-side and keep going even if the
  // operator navigates away (this component unmounts). On return we read the
  // server's `running` map and re-seed the fan-out queue with any in-flight
  // buckets — the queue-follow effect then re-opens SSE and live progress
  // resumes, instead of the dashboard showing nothing. This is the core fix for
  // "scan lost on navigation".
  useEffect(() => {
    refreshScans().then((running) => {
      const inFlight = Object.keys(running);
      if (inFlight.length > 0) {
        setQueue((prev) => (prev.length > 0 ? prev : inFlight));
      }
    });
    listBuckets()
      .then(bs => {
        const names = bs.filter(b => !b.unavailable).map(b => b.name);
        setAllBuckets(names);
        // Seed instant O(1) sizes for every bucket (best-effort; 403 → skip).
        names.forEach(name => {
          getBucketUsage(name)
            .then(u => {
              if (u) setCounters(prev => ({ ...prev, [name]: u }));
            })
            .catch(() => {});
        });
      })
      .catch(() => setAllBuckets([]));
  }, [refreshScans]);

  // Re-poll every 30s for cross-tab scan settling. Routed through
  // useVisiblePolling so a backgrounded tab stops wasting requests (Tier 3.3).
  useVisiblePolling(refreshScans, 30_000);

  // Tear down any SSE on unmount so we don't leak listeners.
  useEffect(() => {
    return () => {
      if (unsubRef.current) unsubRef.current();
      unsubRef.current = null;
    };
  }, []);

  /**
   * Wire SSE for a single bucket. On terminal frame, refresh the
   * cache map and shift the queue head — the next effect tick picks
   * up the new head and continues.
   */
  const subscribe = useCallback((bucket: string) => {
    if (unsubRef.current) unsubRef.current();
    unsubRef.current = subscribeBucketScan(
      bucket,
      frame => {
        setLiveProgress(frame);
        if (frame.finished) {
          refreshScans();
          setLiveProgress(null);
          setQueue(q => q.slice(1));
          if (unsubRef.current) unsubRef.current();
          unsubRef.current = null;
        }
      },
      () => {
        // Transport error: tear down so we don't show a ghost bar.
        setLiveProgress(null);
        if (unsubRef.current) unsubRef.current();
        unsubRef.current = null;
      },
    );
  }, [refreshScans]);

  // Follow the queue head. Idempotent — start endpoint just attaches
  // to an in-flight scan if one is already running on the server.
  useEffect(() => {
    if (queue.length === 0) return;
    const head = queue[0];
    startBucketScan(head).catch(() => {
      // Drop this bucket and try the next.
      setQueue(q => q.slice(1));
    });
    subscribe(head);
  }, [queue, subscribe]);

  /**
   * Default action: scan only the buckets that aren't yet cached.
   * Re-running a full scan on a bucket we already have data for is
   * the wasteful pattern this persistent cache exists to avoid (the
   * "beshu" bucket here is 1.4 TB on Hetzner — 30+ min round trip).
   * If everything is already scanned, we fall through to the
   * explicit "Re-scan all" path below.
   */
  const handleScanMissing = useCallback(() => {
    const missing = allBuckets.filter(b => !scans[b]);
    if (missing.length === 0) return;
    setQueue(missing);
  }, [allBuckets, scans]);

  /**
   * Explicit "really redo every bucket" action — only surfaced when
   * the cache is already complete. Useful when the operator suspects
   * the underlying data drifted and wants fresh numbers.
   */
  const handleRescanAll = useCallback(() => {
    if (allBuckets.length === 0) return;
    setQueue(allBuckets);
  }, [allBuckets]);

  /**
   * Scan a single bucket on demand. Used by the per-row Re-scan
   * affordance in the Top Buckets table — operator clicks the row
   * action and only THAT bucket re-runs. Skips the fan-out queue if
   * something else is already running and instead just enqueues this
   * one for after.
   */
  const handleScanOne = useCallback((bucket: string) => {
    setQueue(q => (q.includes(bucket) ? q : [...q, bucket]));
  }, []);

  /**
   * Cancel the currently-running scan if it's THIS bucket. Otherwise
   * remove the bucket from the queued tail.
   */
  const handleStopOne = useCallback((bucket: string) => {
    setQueue(q => {
      if (q[0] === bucket) {
        // Cancel the live one and drop it from the queue. The next
        // queue head will start automatically.
        stopBucketScan(bucket).catch(() => {});
        return q.slice(1);
      }
      return q.filter(b => b !== bucket);
    });
  }, []);

  const handleStop = useCallback(() => {
    const head = queue[0];
    if (head) stopBucketScan(head).catch(() => {});
    setQueue([]);
    setLiveProgress(null);
    if (unsubRef.current) unsubRef.current();
    unsubRef.current = null;
  }, [queue]);

  /**
   * Synthesise per-bucket rows from the cache map + live progress.
   * The "scanning" flag lights up the row whose SSE is open right
   * now (lets the chart paint a subtle progress underline and the
   * table show "scanning…").
   */
  const bucketRows: BucketRow[] = useMemo(() => {
    const rows: BucketRow[] = allBuckets.map(name => {
      const cached = scans[name];
      const counter = counters[name];
      const isScanning = liveProgress?.bucket === name;
      // Precedence: LIVE scan (ticking) > completed cached scan (ground truth)
      // > O(1) running counter (instant, inline-maintained) > 0. The counter
      // means the row shows a real size immediately, before any scan runs.
      const useLive = isScanning && liveProgress;
      const totalOriginal = useLive
        ? liveProgress!.original_bytes
        : (cached?.total_original_bytes ?? counter?.logical_bytes ?? 0);
      const totalStored = useLive
        ? liveProgress!.stored_bytes
        : (cached?.total_stored_bytes ?? counter?.stored_bytes ?? 0);
      // Route through the canonical scope-savings helper so per-bucket
      // rows share cap + clamp with every other surface. Pre-routing
      // this had its own uncapped formula → a deltaspace with 99.95%
      // raw ratio rendered as 100.0% in the table while the chip
      // (using the helper) showed 99%.
      const scope = summarizeScopeSavings(totalOriginal, totalStored);
      const savings = scope.savedBytes;
      const savingsPercent = scope.pctOneDecimal;
      const objectCount = useLive
        ? liveProgress!.objects
        : (cached?.total_objects ?? counter?.object_count ?? 0);
      return {
        bucket: name,
        totalOriginal,
        totalStored,
        savings,
        savingsPercent,
        objectCount,
        completedAt: cached?.completed_at ?? null,
        scanning: !!isScanning,
        progress: isScanning ? liveProgress! : undefined,
      };
    });
    rows.sort((a, b) => b.totalOriginal - a.totalOriginal);
    return rows;
  }, [allBuckets, scans, liveProgress, counters]);

  const totalOriginal = bucketRows.reduce((s, b) => s + b.totalOriginal, 0);
  const totalStored = bucketRows.reduce((s, b) => s + b.totalStored, 0);
  // HeroSavingsPanel + KPI tile + dial all read these. Use the
  // canonical helper for the % — same cap as everything else; use the
  // signed difference for the cost calculation below (which is purely
  // a denomination conversion, not a display).
  const heroSavings = summarizeScopeSavings(totalOriginal, totalStored);
  const totalSavings = heroSavings.savedBytes;
  const savingsPercent = heroSavings.pctOneDecimal;
  const monthlySavings = (totalSavings / (1024 * 1024 * 1024)) * costRate;
  const totalObjects = bucketRows.reduce((s, b) => s + b.objectCount, 0);

  // Cache-age summary: oldest + newest completed_at across cached rows.
  const cachedRows = bucketRows.filter(r => r.completedAt);
  const unscannedCount = bucketRows.length - cachedRows.length;
  const oldestCompletedAt = cachedRows.length
    ? cachedRows.reduce((min, r) =>
        r.completedAt! < min ? r.completedAt! : min, cachedRows[0].completedAt!)
    : null;
  const newestCompletedAt = cachedRows.length
    ? cachedRows.reduce((max, r) =>
        r.completedAt! > max ? r.completedAt! : max, cachedRows[0].completedAt!)
    : null;
  // Buckets whose cached scan is older than the 6h freshness window. Surfaced
  // as a "stale" nudge to re-scan — we never auto-rescan or delete (the data
  // stays cached on disk; the operator decides when it's worth refreshing).
  const staleCount = cachedRows.filter(r => isScanStale(r.completedAt)).length;

  const opportunities = bucketRows.filter(b => {
    const policy =
      config?.bucket_policies?.[b.bucket] ?? config?.bucket_policies?.[b.bucket.toLowerCase()];
    const bucketCompressionOn = policy?.compression ?? true;
    return !bucketCompressionOn && b.totalOriginal > 1024 * 1024;
  });

  /**
   * Resolve a bucket's backend name from the policy map, falling
   * back to the proxy's default_backend.
   */
  const backendOf = (bucket: string): string | null => {
    const policy =
      config?.bucket_policies?.[bucket] ??
      config?.bucket_policies?.[bucket.toLowerCase()];
    return policy?.backend ?? config?.default_backend ?? null;
  };

  /** The ONE bucket list, sorted under the operator's chosen key. */
  const sortedRows = [...bucketRows].sort((a, b) => {
    switch (topBucketsSort) {
      case 'savings':
        return b.savings - a.savings;
      case 'ratio':
        return b.savingsPercent - a.savingsPercent;
      case 'objects':
        return b.objectCount - a.objectCount;
      case 'recent':
        return (b.completedAt ?? '').localeCompare(a.completedAt ?? '');
      case 'original':
      default:
        return b.totalOriginal - a.totalOriginal;
    }
  });

  // ── Derived facts for the hero footer strip ────────────────────────
  const biggestSaveRow = bucketRows.reduce<BucketRow | null>(
    (best, r) => (r.savings > (best?.savings ?? 0) ? r : best),
    null,
  );
  const totalReference = Object.values(scans).reduce(
    (s, r) => s + (r.total_reference_bytes ?? 0),
    0,
  );
  const referenceShare =
    totalStored > 0 && totalReference > 0 ? totalReference / totalStored : null;

  // ── Opportunity math: dollars left on the table ────────────────────
  // For each compression-OFF bucket, estimate what the fleet's own
  // bytes-weighted ratio would reclaim at the configured cost rate.
  const fleetRatio = savingsPercent / 100;
  const opportunityDollars = opportunities.reduce(
    (s, b) => s + (b.totalOriginal * fleetRatio * costRate) / (1024 * 1024 * 1024),
    0,
  );
  const worstScanned = cachedRows
    .filter(r => r.totalOriginal > 1024 * 1024)
    .reduce<BucketRow | null>(
      (worst, r) => (worst === null || r.savingsPercent < worst.savingsPercent ? r : worst),
      null,
    );

  const isScanning = !!liveProgress || queue.length > 0;

  // Buckets-panel header subline: coverage + freshness in one quiet line.
  const coverageSubtitle = [
    `${cachedRows.length} of ${bucketRows.length} scanned`,
    newestCompletedAt ? `newest ${ageLabel(newestCompletedAt)}` : null,
    oldestCompletedAt && oldestCompletedAt !== newestCompletedAt
      ? `oldest ${ageLabel(oldestCompletedAt)}`
      : null,
    staleCount > 0 ? `${staleCount} stale` : null,
  ]
    .filter(Boolean)
    .join(' · ');

  return (
    <>
      {/* The banner survives only as a live-scan strip — static scan
          plumbing moved into the Buckets panel header so the page
          opens on the money shot, not on cron-job chrome. */}
      {isScanning && (
        <ScanStatusBanner
          colors={colors}
          liveProgress={liveProgress}
          queue={queue}
          cachedCount={cachedRows.length}
          bucketCount={bucketRows.length}
          newestCompletedAt={newestCompletedAt}
          oldestCompletedAt={oldestCompletedAt}
          unscannedCount={unscannedCount}
          staleCount={staleCount}
          allBucketsCount={allBuckets.length}
          scansLoaded={scansLoaded}
          isScanning={isScanning}
          onStop={handleStop}
          onScanMissing={handleScanMissing}
          onRescanAll={handleRescanAll}
        />
      )}

    <DashboardGrid>
      {/* ── Row 1: HERO ─────────────────────────────────────────
          The money shot: multiplier at billboard size in gradient
          ink, before/after two-bar proof (ghost "without" + the real
          bar where luminous green = saved), dollar line ($/mo + $/yr),
          and one quiet footer strip of derived facts. Animation fires
          once per session; honours prefers-reduced-motion.
      */}
      <Panel title="Total savings" colSpan={12} rowSpan={3} accent="green">
        <HeroSavingsPanel
          totalOriginal={totalOriginal}
          totalStored={totalStored}
          savingsPercent={savingsPercent}
          monthlySavings={monthlySavings}
          costRate={costRate}
          onChangeCostRate={saveCostRate}
          totalObjects={totalObjects}
          bucketCount={bucketRows.length}
          biggestSave={
            biggestSaveRow && biggestSaveRow.savings > 0
              ? { bucket: biggestSaveRow.bucket, bytes: biggestSaveRow.savings }
              : null
          }
          referenceShare={referenceShare}
          unscannedCount={unscannedCount}
          onScanMissing={handleScanMissing}
          liveScanning={!!liveProgress}
        />
      </Panel>

      {/* ── Row 2: THE bucket list ───────────────────────────────
          One full-width list replaces the old fleet view + Top-5
          table (which showed the same rows twice). Scan plumbing
          (coverage subline, sort, Scan missing / Re-scan all) lives
          in this panel's header.
      */}
      <Panel
        title="Buckets"
        subtitle={
          bucketRows.length > 0
            ? `${coverageSubtitle} · sorted by ${SORT_LABELS[topBucketsSort]}`
            : undefined
        }
        colSpan={12}
        rowSpan={(bucketRows.length <= 3 ? 1 : bucketRows.length <= 7 ? 2 : 3) as 1 | 2 | 3}
        empty={
          bucketRows.length === 0
            ? {
                title: 'No buckets yet',
                hint: 'Create a bucket and upload a few objects to populate analytics.',
              }
            : undefined
        }
        actions={
          bucketRows.length > 0 ? (
            <>
              <TopBucketsSortSelect
                value={topBucketsSort}
                onChange={setTopBucketsSort}
                colors={colors}
              />
              {unscannedCount > 0 && (
                <Button
                  size="small"
                  type="primary"
                  icon={<CaretRightOutlined />}
                  onClick={handleScanMissing}
                  disabled={!scansLoaded || isScanning}
                  title={`Scan the ${unscannedCount} bucket${unscannedCount === 1 ? '' : 's'} with no cached scan`}
                >
                  Scan missing ({unscannedCount})
                </Button>
              )}
              <Button
                size="small"
                icon={<ReloadOutlined />}
                onClick={handleRescanAll}
                disabled={!scansLoaded || isScanning}
                title="Re-scan every bucket"
              >
                Re-scan all
              </Button>
            </>
          ) : undefined
        }
      >
        {bucketRows.length > 0 && (
          <BucketFleetList
            bucketRows={sortedRows}
            colors={colors}
            config={config}
            queue={queue}
            scansLoaded={scansLoaded}
            backendOf={backendOf}
            onStopOne={handleStopOne}
            onScanOne={handleScanOne}
          />
        )}
      </Panel>

      {/* ── Row 3: Opportunity + Coverage (only once data exists) ── */}
      {cachedRows.length > 0 && (
        <>
          <Panel
            title="Left on the table"
            subtitle={
              opportunities.length > 0
                ? `${opportunities.length} bucket${opportunities.length === 1 ? '' : 's'} with compression OFF`
                : 'Every bucket is compressing'
            }
            colSpan={8}
            rowSpan={2}
            accent={opportunities.length > 0 ? 'amber' : undefined}
          >
            {opportunities.length > 0 ? (
              <div style={{ display: 'flex', flexDirection: 'column', gap: 12 }}>
                <div
                  style={{
                    fontSize: 22,
                    fontWeight: 800,
                    color: colors.ACCENT_AMBER,
                    fontFamily: 'var(--font-ui)',
                    fontVariantNumeric: 'tabular-nums',
                    letterSpacing: '-0.02em',
                  }}
                >
                  est. +${opportunityDollars.toFixed(2)}/mo
                  <span style={{ fontSize: 13, fontWeight: 500, color: colors.TEXT_MUTED, marginLeft: 8 }}>
                    if these buckets compressed at the fleet ratio
                  </span>
                </div>
                {opportunities.map(b => (
                  <div
                    key={b.bucket}
                    style={{
                      display: 'flex',
                      justifyContent: 'space-between',
                      gap: 12,
                      fontFamily: 'var(--font-mono)',
                      fontSize: 12,
                      color: colors.TEXT_PRIMARY,
                      borderTop: `1px solid ${colors.BORDER}`,
                      paddingTop: 8,
                    }}
                  >
                    <span>{b.bucket}</span>
                    <span style={{ color: colors.TEXT_MUTED }}>
                      {formatBytes(b.totalOriginal)} · est. +$
                      {((b.totalOriginal * fleetRatio * costRate) / 1024 ** 3).toFixed(2)}/mo
                    </span>
                  </div>
                ))}
                <div style={{ fontSize: 11, color: colors.TEXT_MUTED, fontFamily: 'var(--font-ui)' }}>
                  Compression is a per-bucket policy — flip it on under Storage → Buckets.
                </div>
              </div>
            ) : (
              <div
                style={{
                  flex: 1,
                  display: 'flex',
                  flexDirection: 'column',
                  justifyContent: 'center',
                  gap: 8,
                  fontFamily: 'var(--font-ui)',
                }}
              >
                <div style={{ fontSize: 16, fontWeight: 700, color: colors.SAVED_TEXT }}>
                  Nothing left on the table
                </div>
                <div style={{ fontSize: 12, color: colors.TEXT_MUTED, maxWidth: 480, lineHeight: 1.5 }}>
                  Compression is enabled on every bucket.
                  {worstScanned && worstScanned.savingsPercent < 20 && (
                    <>
                      {' '}
                      Weakest ratio: <span style={{ fontFamily: 'var(--font-mono)' }}>{worstScanned.bucket}</span>{' '}
                      at {worstScanned.savingsPercent.toFixed(1)}% — content that's already
                      compressed or rarely versioned won't delta well.
                    </>
                  )}
                </div>
              </div>
            )}
          </Panel>

          <Panel title="Scan coverage" subtitle="Cached results survive restarts" colSpan={4} rowSpan={2}>
            <CoverageStrip
              colors={colors}
              freshCount={cachedRows.length - staleCount}
              staleCount={staleCount}
              unscannedCount={unscannedCount}
              newestCompletedAt={newestCompletedAt}
              oldestCompletedAt={oldestCompletedAt}
            />
          </Panel>
        </>
      )}
    </DashboardGrid>
    </>
  );
}

/**
 * Segmented coverage strip: fresh | stale | unscanned, with counts and
 * the oldest/newest scan ages. Replaces the old gauge (which repeated
 * the hero number at a fifth of the size).
 */
function CoverageStrip({
  colors,
  freshCount,
  staleCount,
  unscannedCount,
  newestCompletedAt,
  oldestCompletedAt,
}: {
  colors: ReturnType<typeof useColors>;
  freshCount: number;
  staleCount: number;
  unscannedCount: number;
  newestCompletedAt: string | null;
  oldestCompletedAt: string | null;
}) {
  const total = Math.max(1, freshCount + staleCount + unscannedCount);
  const seg = (n: number) => `${(n / total) * 100}%`;
  const rows: Array<{ label: string; count: number; color: string }> = [
    { label: 'fresh', count: freshCount, color: colors.ACCENT_GREEN },
    { label: 'stale (>6h)', count: staleCount, color: colors.ACCENT_AMBER },
    { label: 'not scanned', count: unscannedCount, color: colors.TEXT_FAINT },
  ];
  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 14, fontFamily: 'var(--font-ui)' }}>
      <div
        style={{
          display: 'flex',
          height: 14,
          borderRadius: 5,
          overflow: 'hidden',
          border: `1px solid ${colors.BORDER}`,
          background: colors.BAR_TRACK,
        }}
      >
        {rows.map(
          r =>
            r.count > 0 && (
              <div
                key={r.label}
                style={{ width: seg(r.count), background: r.color, opacity: r.label === 'not scanned' ? 0.45 : 0.85 }}
                title={`${r.count} ${r.label}`}
              />
            ),
        )}
      </div>
      <div style={{ display: 'flex', flexDirection: 'column', gap: 8 }}>
        {rows.map(r => (
          <div
            key={r.label}
            style={{ display: 'flex', alignItems: 'center', gap: 8, fontSize: 12, color: colors.TEXT_SECONDARY }}
          >
            <span
              style={{ width: 8, height: 8, borderRadius: 2, background: r.color, opacity: r.label === 'not scanned' ? 0.45 : 0.85 }}
            />
            <span style={{ flex: 1 }}>{r.label}</span>
            <span style={{ fontFamily: 'var(--font-mono)', fontVariantNumeric: 'tabular-nums', color: colors.TEXT_PRIMARY, fontWeight: 600 }}>
              {r.count}
            </span>
          </div>
        ))}
      </div>
      <div style={{ fontSize: 11, color: colors.TEXT_MUTED, lineHeight: 1.5 }}>
        {newestCompletedAt ? (
          <>
            Newest scan {ageLabel(newestCompletedAt)}
            {oldestCompletedAt && oldestCompletedAt !== newestCompletedAt && (
              <> · oldest {ageLabel(oldestCompletedAt)}</>
            )}
          </>
        ) : (
          'No scans yet'
        )}
      </div>
    </div>
  );
}
