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
import { XAxis, YAxis, ResponsiveContainer, Tooltip as RechartsTooltip, AreaChart, Area } from 'recharts';
import { useColors } from '../ThemeContext';
import { listBuckets } from '../s3client';
import type { AdminConfig } from '../adminApi';
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
import { CHART_PALETTE, chartTooltipStyle, axisTickStyle, fmtNum } from './dashboard/chartDefaults';
import HeroSavingsPanel from './HeroSavingsPanel';
import ByTheNumbersGrid from './ByTheNumbersGrid';
import FleetHealthGauge from './FleetHealthGauge';
import TopBucketsSortSelect from './TopBucketsSortSelect';
import BucketFleetList from './BucketFleetList';
import TopBucketsList from './TopBucketsList';
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
  const tt = chartTooltipStyle(colors);

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

  /**
   * Live scan timeline — one point per SSE frame for the bucket
   * currently being scanned. Resets when a new bucket starts.
   * Powers the "Scan progress" chart that replaces the old session-
   * savings chart (which only made sense for the polling-based
   * /stats fetch we just removed).
   */
  const [scanTimeline, setScanTimeline] = useState<
    Array<{ time: string; objects: number; bytes: number }>
  >([]);

  useEffect(() => {
    if (!liveProgress) {
      setScanTimeline([]);
      return;
    }
    setScanTimeline(prev => {
      const now = new Date().toLocaleTimeString([], {
        hour: '2-digit', minute: '2-digit', second: '2-digit',
      });
      // Reset the timeline when a new bucket takes over.
      if (prev.length > 0 && liveProgress.objects < prev[prev.length - 1].objects) {
        return [{ time: now, objects: liveProgress.objects, bytes: liveProgress.original_bytes }];
      }
      return [...prev, {
        time: now,
        objects: liveProgress.objects,
        bytes: liveProgress.original_bytes,
      }].slice(-60); // keep the last minute-or-so of frames
    });
  }, [liveProgress]);

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
      .then(bs => setAllBuckets(bs.map(b => b.name)))
      .catch(() => setAllBuckets([]));
    const id = window.setInterval(refreshScans, 30_000);
    return () => window.clearInterval(id);
  }, [refreshScans]);

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
      const isScanning = liveProgress?.bucket === name;
      // While a bucket is being scanned, prefer the LIVE counters so
      // the user sees numbers ticking up rather than yesterday's
      // cached totals.
      const useLive = isScanning && liveProgress;
      const totalOriginal = useLive
        ? liveProgress!.original_bytes
        : (cached?.total_original_bytes ?? 0);
      const totalStored = useLive
        ? liveProgress!.stored_bytes
        : (cached?.total_stored_bytes ?? 0);
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
        : (cached?.total_objects ?? 0);
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
  }, [allBuckets, scans, liveProgress]);

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

  // Top-5 table (or fewer if <5 buckets). Renders next to the fleet view.
  /**
   * Resolve a bucket's backend name from the policy map, falling
   * back to the proxy's default_backend. The backend label is shown
   * as a small chip next to the bucket name in Top buckets so the
   * operator can tell at a glance which storage tier a bucket lives
   * on (Hetzner vs AWS vs filesystem etc.).
   */
  const backendOf = (bucket: string): string | null => {
    const policy =
      config?.bucket_policies?.[bucket] ??
      config?.bucket_policies?.[bucket.toLowerCase()];
    return policy?.backend ?? config?.default_backend ?? null;
  };

  /**
   * Sorted top-buckets list. Compares all rows under the chosen
   * sort key — there's no point capping at 5 only to then sort that
   * partial slice. The render still slices to top 5.
   */
  const topBuckets = [...bucketRows]
    .sort((a, b) => {
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
    })
    .slice(0, 5);

  const isScanning = !!liveProgress || queue.length > 0;

  return (
    <>
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

    <DashboardGrid>
      {/* ── Row 1: HERO ─────────────────────────────────────────
          One 12-col / 3-row panel replaces the old four-card KPI
          strip. The hero composition (HeroSavingsPanel) shows the
          % saved at billboard size, the scale-accurate before/after
          bar, the dollar figure with a count-up animation, and the
          cache-age line — all in one read. Animation fires once per
          session via sessionStorage; honours prefers-reduced-motion.
      */}
      <Panel
        title="Total savings"
        subtitle={
          totalObjects > 0
            ? `${fmtNum(totalObjects)} objects across ${bucketRows.length} bucket${bucketRows.length === 1 ? '' : 's'}`
            : undefined
        }
        colSpan={12}
        rowSpan={3}
        accent="green"
      >
        <HeroSavingsPanel
          totalOriginal={totalOriginal}
          totalStored={totalStored}
          savingsPercent={savingsPercent}
          monthlySavings={monthlySavings}
          costRate={costRate}
          onChangeCostRate={saveCostRate}
          cacheAgeNewest={newestCompletedAt ? ageLabel(newestCompletedAt) : null}
          cacheAgeOldest={oldestCompletedAt ? ageLabel(oldestCompletedAt) : null}
          unscannedCount={unscannedCount}
          liveScanning={!!liveProgress}
        />
      </Panel>

      {/* ── Row 2: Bucket fleet view + Top-5 table ───────────── */}
      {/*
        The old "Storage by bucket" recharts stacked bar collapsed
        into a single line when one bucket dwarfed the rest (beshu @
        1.4 TB next to dgp-conf @ 88 KB → small buckets invisible).
        This replacement gives each bucket TWO bars per row:
         - **Ratio bar** (always full-width): the kept|saved split
           in the same teal+dotted style as the hero panel. Tells the
           viewer "how well is THIS bucket compressing" regardless
           of its absolute size.
         - **Footprint bar** (scale-relative): a thin under-bar
           showing this bucket's original-size share of the largest
           bucket. Preserves the magnitude story.
        Sorted by original size, descending. No overlapping axis
        labels because every bucket name renders ABOVE its row, not
        inside a chart axis.
      */}
      <Panel
        title="Bucket fleet"
        subtitle="Ratio per bucket · footprint relative to largest"
        colSpan={8}
        // Panel height adapts to bucket count: 1 row for 1-2 buckets,
        // 2 rows for 3-6, 3 rows beyond. Avoids the giant empty void
        // beneath the rows when the fleet has only a handful of
        // entries (the rowSpan was always 3 even with 3 buckets).
        rowSpan={(bucketRows.length <= 2 ? 1 : bucketRows.length <= 6 ? 2 : 3) as 1 | 2 | 3}
        empty={bucketRows.length === 0 ? { title: 'No bucket data yet', hint: 'Create a bucket and upload a few objects to populate analytics.' } : undefined}
      >
        {bucketRows.length > 0 && (
          <BucketFleetList bucketRows={bucketRows} colors={colors} />
        )}
      </Panel>
      <Panel
        title="Top buckets"
        subtitle={`Sorted by ${SORT_LABELS[topBucketsSort]}`}
        colSpan={4}
        // Mirror Bucket fleet's adaptive height so they end the same
        // row together — no orphan column trailing into empty space.
        rowSpan={(bucketRows.length <= 2 ? 1 : bucketRows.length <= 6 ? 2 : 3) as 1 | 2 | 3}
        empty={topBuckets.length === 0 ? { title: 'No buckets' } : undefined}
        actions={topBuckets.length > 0 ? (
          <TopBucketsSortSelect value={topBucketsSort} onChange={setTopBucketsSort} colors={colors} />
        ) : undefined}
      >
        {topBuckets.length > 0 && (
          <TopBucketsList
            topBuckets={topBuckets}
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

      {/* ── Row 3: By the numbers · Compression health · Live scan ──
          The old row was empty 90% of the time ("No scan running" /
          "Nothing to flag"). Replaced with derived facts that
          ALWAYS have content, plus a live-stream panel that takes
          over the whole row when a scan is actively in flight.
      */}
      {liveProgress ? (
        // Active-scan mode: dedicate the row to the streaming chart.
        <Panel
          title={`Scan progress · ${liveProgress.bucket}`}
          subtitle={`${fmtNum(liveProgress.objects)} objects · ${formatBytes(liveProgress.original_bytes)} seen · ${liveProgress.pages_done} pages`}
          colSpan={12}
          rowSpan={2}
          accent="blue"
          empty={scanTimeline.length < 2 ? { title: 'Streaming…', hint: 'First point lands after a few pages.' } : undefined}
        >
          {scanTimeline.length >= 2 && (
            <div style={{ flex: 1, minHeight: 0 }}>
              <ResponsiveContainer width="100%" height="100%">
                <AreaChart data={scanTimeline} margin={{ top: 8, right: 8, bottom: 0, left: -20 }}>
                  <XAxis dataKey="time" tick={axisTickStyle(colors, true)} axisLine={false} tickLine={false} minTickGap={40} />
                  <YAxis tickFormatter={v => fmtNum(Number(v))} tick={axisTickStyle(colors)} axisLine={false} tickLine={false} width={56} />
                  <RechartsTooltip
                    {...tt}
                    formatter={(v, name) =>
                      name === 'objects'
                        ? [fmtNum(Number(v)), 'Objects']
                        : [formatBytes(Number(v)), 'Bytes']
                    }
                  />
                  <Area type="monotone" dataKey="objects" stroke={CHART_PALETTE[5]} fill={`${CHART_PALETTE[5]}33`} strokeWidth={2} />
                </AreaChart>
              </ResponsiveContainer>
            </div>
          )}
        </Panel>
      ) : (
        <>
          {/* By-the-numbers facts grid — always renders, no empty states. */}
          <Panel
            title="By the numbers"
            subtitle="Derived facts from the latest scan"
            colSpan={8}
            rowSpan={2}
          >
            <ByTheNumbersGrid bucketRows={bucketRows} colors={colors} />
          </Panel>
          {/*
            Compression effectiveness gauge. The dial is BYTES-WEIGHTED
            (a 1.4 TB bucket at 93% dominates a 88 KB bucket at 0%);
            this matches the user's intuition of "how is my storage
            cost behaving" rather than the unweighted average that
            penalises trivial buckets. The subtitle counts the
            operator-policy warnings — buckets where compression is
            explicitly OFF — as a separate signal from compression
            effectiveness, because "compression turned off on
            purpose" is a different category from "compression on
            but ratio is low".
          */}
          <Panel
            title="Compression effectiveness"
            subtitle={
              opportunities.length > 0
                ? `${opportunities.length} bucket${opportunities.length === 1 ? '' : 's'} with compression OFF`
                : 'Bytes-weighted across buckets'
            }
            colSpan={4}
            rowSpan={2}
            accent={opportunities.length > 0 ? 'amber' : undefined}
          >
            <FleetHealthGauge
              bucketRows={bucketRows}
              opportunities={opportunities}
              colors={colors}
            />
          </Panel>
        </>
      )}

      {/*
        Row 4 (Per-bucket savings %) removed — the Bucket fleet panel
        in row 2 now shows the per-bucket ratio with its 100%-wide
        rendering, so a separate distribution chart was redundant.
      */}
    </DashboardGrid>
    </>
  );
}
