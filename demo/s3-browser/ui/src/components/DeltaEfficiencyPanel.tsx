/**
 * DeltaEfficiencyPanel — bucket-scoped diagnostics surface.
 *
 * Backs onto `GET /_/api/admin/diagnostics/delta-efficiency` (see
 * `src/api/admin/delta_efficiency.rs`). Scans every deltaspace in
 * the chosen bucket and renders per-prefix verdicts so the operator
 * can spot prefixes whose reference baseline is wrong (a poor seed
 * choice produces deltas nearly the size of the originals, wasting
 * storage).
 *
 * Read-only. Operator action is "re-upload the prefix" — offered as
 * a copyable s3:// URI rather than performed here, since that's a
 * destructive operation we don't want one-click.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { Typography, Button, Tag, Alert, Space, Select, InputNumber, Spin } from 'antd';
import { ReloadOutlined, ThunderboltOutlined, CopyOutlined } from '@ant-design/icons';
import { useColors } from '../ThemeContext';
import { clamp, formatBytes } from '../utils';
import {
  fetchDeltaEfficiency,
  triggerDeltaEfficiencyScan,
  verifyDeltaEfficiency,
  getBucketOrigins,
  type DeltaEfficiencyResponse,
  type DeltaspaceEfficiencyReport,
  type DeltaEfficiency,
  type VerifyDeltaEfficiencyResponse,
} from '../adminApi';
import HoverHint from './HoverHint';

/**
 * Per-row verification state, keyed by prefix. Stored at the
 * dashboard level so re-renders don't lose in-flight HEAD scans and
 * results survive scrolling / filter toggles.
 */
type VerifyState =
  | { status: 'loading' }
  | { status: 'ok'; result: VerifyDeltaEfficiencyResponse }
  | { status: 'error'; message: string };

const { Text, Paragraph } = Typography;

interface Props {
  onSessionExpired?: () => void;
}

function efficiencyLabel(e: DeltaEfficiency): string {
  switch (e) {
    case 'excellent': return 'Excellent';
    case 'good': return 'Good';
    case 'fair': return 'Fair';
    case 'poor': return 'Poor';
    case 'no_reference': return 'No reference';
  }
}

function copyToClipboard(text: string) {
  if (navigator.clipboard && window.isSecureContext) {
    navigator.clipboard.writeText(text);
  }
}

/**
 * Poll cadence on 202 Accepted. Optimized backend lands in ~3-5s for a
 * 141-prefix bucket, so 3s catches the answer in a single tick. Bumping
 * higher would slow the UX on the common case; lower would hammer the
 * server for no benefit.
 */
const POLL_INTERVAL_MS = 3000;

/**
 * Ceiling for the 202 → 200 polling loop. Sized for buckets with
 * thousands of deltaspaces; the optimized scanner lands inside this
 * window comfortably for any realistic bucket.
 */
const SCAN_TIMEOUT_MS = 300_000;

export default function DeltaEfficiencyPanel({ onSessionExpired }: Props) {
  const colors = useColors();
  const [buckets, setBuckets] = useState<string[]>([]);
  const [bucket, setBucket] = useState<string | undefined>(undefined);
  const [minDeltas, setMinDeltas] = useState<number>(3);
  const [response, setResponse] = useState<DeltaEfficiencyResponse | null>(null);
  const [loading, setLoading] = useState(false);
  const [scanning, setScanning] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // Monotonic id stamped on each scan. A polling loop checks this
  // against the latest id before publishing state, so a stale scan
  // (e.g. bucket A still polling when the operator switches to bucket
  // B) can't overwrite the newer scan's result. See runScan.
  const scanIdRef = useRef(0);

  // Load bucket list on mount.
  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const origins = await getBucketOrigins();
        if (cancelled) return;
        const names = origins.buckets.map((b: { name: string }) => b.name).sort();
        setBuckets(names);
        if (names.length > 0 && bucket === undefined) {
          setBucket(names[0]);
        }
      } catch (e) {
        if (cancelled) return;
        const msg = e instanceof Error ? e.message : String(e);
        if (/401|session/i.test(msg)) {
          onSessionExpired?.();
        }
        setError(`Could not load bucket list: ${msg}`);
      }
    })();
    return () => { cancelled = true; };
  }, [onSessionExpired]); // eslint-disable-line react-hooks/exhaustive-deps

  /**
   * Server returns one of:
   *   - 200 OK with the report (cached or fresh)
   *   - 202 Accepted with `{scanning: true}` and a background scan
   *     was just enqueued (or one was already running).
   *
   * On 202, poll the GET endpoint at `POLL_INTERVAL_MS` cadence until
   * we get a 200, bounded by `SCAN_TIMEOUT_MS`.
   */
  const fetchOrPoll = async (
    forceRescan: boolean,
    scanId: number,
  ): Promise<DeltaEfficiencyResponse | null> => {
    if (!bucket) return null;
    if (forceRescan) {
      // Force a re-scan, then poll.
      await triggerDeltaEfficiencyScan(bucket, minDeltas);
    }
    const startedAt = Date.now();
    const deadline = startedAt + SCAN_TIMEOUT_MS;
    // First call: if fresh-cached we'll get 200 immediately.
    // If not, the server enqueues a scan and we get 202.
    while (Date.now() < deadline) {
      const r = await fetchDeltaEfficiency(bucket, minDeltas);
      // A newer scan superseded us — stop polling and discard the
      // result so it can't clobber the newer scan's state.
      if (scanIdRef.current !== scanId) return null;
      if ('scanning' in r) {
        // Server is working on it. Wait briefly, then re-fetch.
        setScanning(true);
        await new Promise(resolve => setTimeout(resolve, POLL_INTERVAL_MS));
        continue;
      }
      setScanning(false);
      return r;
    }
    throw new Error(
      `Scan timed out after ${Math.round(SCAN_TIMEOUT_MS / 1000)}s — ` +
        `try again or check server logs`,
    );
  };

  const runScan = async (forceRescan = false) => {
    if (!bucket) return;
    // Stamp this scan. Any older in-flight poll loop will see a newer
    // id and bail before publishing, so out-of-order completions can't
    // overwrite the latest scan's result.
    const scanId = ++scanIdRef.current;
    setLoading(true);
    setError(null);
    if (forceRescan) setResponse(null);
    try {
      const r = await fetchOrPoll(forceRescan, scanId);
      if (scanIdRef.current !== scanId) return;
      if (r) setResponse(r);
    } catch (e) {
      if (scanIdRef.current !== scanId) return;
      const msg = e instanceof Error ? e.message : String(e);
      if (/401|session/i.test(msg)) onSessionExpired?.();
      setError(`Scan failed: ${msg}`);
    } finally {
      // Only the latest scan owns the shared loading/scanning flags;
      // a superseded loop must not flip them off under the new scan.
      if (scanIdRef.current === scanId) {
        setLoading(false);
        setScanning(false);
      }
    }
  };

  // Summary counts, derived once per response change.
  const summary = useMemo(() => {
    if (!response) return null;
    const counts: Record<DeltaEfficiency, number> = {
      excellent: 0,
      good: 0,
      fair: 0,
      poor: 0,
      no_reference: 0,
    };
    let totalWasted = 0;
    for (const r of response.reports) {
      counts[r.efficiency] += 1;
      if (r.efficiency === 'poor' || r.efficiency === 'no_reference') {
        totalWasted += r.total_delta_bytes;
      }
    }
    return { counts, totalWasted };
  }, [response]);

  return (
    <div style={{ padding: '16px 24px' }}>
      <Paragraph style={{ marginBottom: 12, color: colors.TEXT_SECONDARY }}>
        Scan a bucket and surface deltaspaces where the reference
        baseline doesn't compress its siblings — typically the median
        delta is the same size as the reference, or larger. Re-uploading
        such a prefix lets the proxy pick a better seed and recovers
        most of the stored bytes. Read-only diagnostic; you decide what
        to re-upload.
      </Paragraph>

      <Space wrap style={{ marginBottom: 16 }}>
        <Select
          style={{ minWidth: 280 }}
          placeholder="Select bucket"
          value={bucket}
          onChange={setBucket}
          options={buckets.map(b => ({ value: b, label: b }))}
          showSearch
        />
        <HoverHint hint="Skip prefixes with fewer than this many deltas. Smaller deltaspaces don't have enough signal to draw a verdict from.">
          <span>
            min deltas:{' '}
            <InputNumber
              size="small"
              min={1}
              max={1000}
              value={minDeltas}
              onChange={v => setMinDeltas(typeof v === 'number' ? v : 3)}
              style={{ width: 70 }}
            />
          </span>
        </HoverHint>
        <Button
          type="primary"
          icon={<ThunderboltOutlined />}
          onClick={() => runScan(false)}
          loading={loading && !scanning}
          disabled={!bucket}
        >
          {response ? 'Refresh' : 'Scan'}
        </Button>
        {response && (
          <HoverHint hint="Force a fresh scan, ignoring the 5-min cache.">
            <Button
              icon={<ReloadOutlined />}
              onClick={() => runScan(true)}
              loading={loading}
            >
              Re-scan
            </Button>
          </HoverHint>
        )}
        {scanning && (
          <Space>
            <Spin size="small" />
            <Text type="secondary">Scan running on the server, polling…</Text>
          </Space>
        )}
      </Space>

      {error && (
        <Alert
          type="error"
          message="Error"
          description={error}
          style={{ marginBottom: 16 }}
          showIcon
        />
      )}

      {response && summary && (
        <>
          {response.reports.length === 0 ? (
            <Alert
              type="success"
              message="No deltaspaces met the reporting threshold"
              description={`No prefix in '${response.bucket}' has at least ${response.min_deltas} deltas. Try lowering 'min deltas' or pick another bucket.`}
              showIcon
            />
          ) : (
            <EfficiencyDashboard
              response={response}
              counts={summary.counts}
              totalRecoverableBytes={summary.totalWasted}
            />
          )}
        </>
      )}
    </div>
  );
}

/* ────────────────────────────────────────────────────────────────
   EfficiencyDashboard — reimagined display.

   The original flat table buried the one number that matters
   (median delta as a fraction of reference) under a wall of
   absolute byte values. The redesign foregrounds the ratio and
   sorts hardest-broken first. Three bands, top to bottom:

     1. HeroHeadline — one actionable sentence with the
        recoverable-bytes figure.
     2. DistributionStackBar — a single horizontal stacked bar
        showing the verdict mix at a glance.
     3. RankedRatioRows — one row per prefix, the median-ratio
        rendered as a colored bar against a 0–100% axis with
        threshold tick marks at 20% (Fair) and 50% (Poor).

   What's deliberately gone:
     - Median/Max/Total/Original/Saved columns (noise — they
       don't change the decision)
     - Five-colored count chips (replaced by the stack bar)
     - Verbose "Why / suggestion" copy (now in a HoverHint)
   ──────────────────────────────────────────────────────────────── */

function EfficiencyDashboard({
  response,
  counts,
  totalRecoverableBytes,
}: {
  response: DeltaEfficiencyResponse;
  counts: Record<DeltaEfficiency, number>;
  totalRecoverableBytes: number;
}) {
  const colors = useColors();
  const [regressionsOnly, setRegressionsOnly] = useState(false);
  // Per-prefix verification state, keyed by full s3:// URI. Cleared on
  // response.bucket change by the effect below (the dashboard does NOT
  // remount on a bucket switch, despite earlier comments to the contrary).
  const [verifyState, setVerifyState] = useState<Map<string, VerifyState>>(new Map());

  // The dashboard does NOT remount on a bucket switch (it's rendered without a
  // key and `response` isn't nulled on switch), so a verify result from the
  // previous bucket would linger. Clear the Map whenever the response's bucket
  // changes — `response.bucket` is the in-scope, canonical signal here.
  useEffect(() => {
    setVerifyState(new Map());
  }, [response.bucket]);

  const totalReported = response.reported_deltaspaces;
  const actionable = counts.poor + counts.no_reference;

  const visibleReports = regressionsOnly
    ? response.reports.filter((r) =>
        r.efficiency === 'poor' ||
        r.efficiency === 'no_reference' ||
        r.efficiency === 'fair',
      )
    : response.reports;

  const runVerify = useCallback(
    async (bucket: string, prefix: string) => {
      const key = `${bucket}/${prefix}`;
      setVerifyState((prev) => new Map(prev).set(key, { status: 'loading' }));
      try {
        const result = await verifyDeltaEfficiency(bucket, prefix);
        setVerifyState((prev) => new Map(prev).set(key, { status: 'ok', result }));
      } catch (e) {
        const message = e instanceof Error ? e.message : String(e);
        setVerifyState((prev) =>
          new Map(prev).set(key, { status: 'error', message }),
        );
      }
    },
    [],
  );

  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 16 }}>
      {/* ── Band 1: HeroHeadline ─────────────────────────────────── */}
      <HeroHeadline
        actionable={actionable}
        totalReported={totalReported}
        totalRecoverableBytes={totalRecoverableBytes}
        scannedAt={response.computed_at}
        cached={response.cached}
        minDeltas={response.min_deltas}
      />

      {/* ── Band 2: DistributionStackBar ─────────────────────────── */}
      <DistributionStackBar counts={counts} total={totalReported} />

      {/* ── Band 2.5: Filter toggle + count ──────────────────────── */}
      <div
        style={{
          display: 'flex',
          alignItems: 'center',
          justifyContent: 'space-between',
          fontSize: 12,
          color: colors.TEXT_SECONDARY,
        }}
      >
        <span>
          Showing {visibleReports.length.toLocaleString()} of{' '}
          {totalReported.toLocaleString()} prefixes
        </span>
        <label style={{ display: 'inline-flex', alignItems: 'center', gap: 6, cursor: 'pointer' }}>
          <input
            type="checkbox"
            checked={regressionsOnly}
            onChange={(e) => setRegressionsOnly(e.target.checked)}
            style={{ cursor: 'pointer' }}
          />
          <span>Regressions only (Poor / Fair / No-ref)</span>
        </label>
      </div>

      {/* ── Verify-savings explainer ─────────────────────────────── */}
      <VerifyExplainer />

      {/* ── Band 3: RankedRatioRows ──────────────────────────────── */}
      <RankedRatioRows
        reports={visibleReports}
        verifyState={verifyState}
        onVerify={runVerify}
      />
    </div>
  );
}

/* ── VerifyExplainer — one-line note before the rows table ──────── */

function VerifyExplainer() {
  const colors = useColors();
  return (
    <div
      style={{
        display: 'flex',
        gap: 10,
        padding: '10px 14px',
        background: `${colors.ACCENT_BLUE}0c`,
        border: `1px dashed ${colors.ACCENT_BLUE}55`,
        borderRadius: 8,
        fontSize: 12,
        color: colors.TEXT_SECONDARY,
        lineHeight: 1.5,
      }}
    >
      <span
        style={{
          fontSize: 14,
          color: colors.ACCENT_BLUE,
          flexShrink: 0,
          lineHeight: '18px',
        }}
        aria-hidden
      >
        ⓘ
      </span>
      <div>
        <b style={{ color: colors.TEXT_PRIMARY }}>What you're seeing is a fast proxy metric</b> —
        the ratio of each prefix's typical delta to its reference baseline.
        It flags wrong-baseline cases reliably but
        {' '}<b>can't tell you whether DG is actually saving bytes versus storing
        originals as-is</b>, because that needs per-file HEAD calls the bulk
        scan deliberately skips.
        <br />
        Click <b>▶ Verify</b> on any row to fire those HEAD calls just for
        that prefix (~1 second for a few hundred files). You'll get the true
        savings number — positive (DG is helping) or negative (you'd be
        better off without DG on this prefix).
      </div>
    </div>
  );
}

/* ── Band 1: HeroHeadline ───────────────────────────────────────── */

function HeroHeadline({
  actionable,
  totalReported,
  totalRecoverableBytes,
  scannedAt,
  cached,
  minDeltas,
}: {
  actionable: number;
  totalReported: number;
  totalRecoverableBytes: number;
  scannedAt: string;
  cached: boolean;
  minDeltas: number;
}) {
  const colors = useColors();
  const allHealthy = actionable === 0;
  const headlineText = allHealthy
    ? 'All prefixes are compressing well.'
    : `${actionable} of ${totalReported.toLocaleString()} prefixes need re-baselining`;
  const subText = allHealthy
    ? `Scanned ${totalReported.toLocaleString()} deltaspace(s) with ≥${minDeltas} deltas. No regression found.`
    : `≈ ${formatBytes(totalRecoverableBytes)} stored as bad-reference deltas. Re-upload the listed prefixes to recover.`;

  return (
    <div
      style={{
        padding: '18px 22px',
        background: allHealthy
          ? `${colors.ACCENT_BLUE}10`
          : `${colors.ACCENT_RED}12`,
        border: `1px solid ${allHealthy ? colors.ACCENT_BLUE : colors.ACCENT_RED}40`,
        borderRadius: 10,
      }}
    >
      <div
        style={{
          fontSize: 20,
          fontWeight: 600,
          color: allHealthy ? colors.ACCENT_BLUE : colors.ACCENT_RED,
          marginBottom: 6,
          letterSpacing: -0.2,
        }}
      >
        {headlineText}
      </div>
      <div style={{ fontSize: 13, color: colors.TEXT_SECONDARY }}>
        {subText}
        {cached && (
          <HoverHint hint={`Scan completed at ${scannedAt}. Cached for 5 min — click Re-scan for fresh data.`}>
            <Tag
              color="blue"
              style={{ marginLeft: 8, fontSize: 11, padding: '0 6px', lineHeight: '18px' }}
            >
              cached
            </Tag>
          </HoverHint>
        )}
      </div>
    </div>
  );
}

/* ── Band 2: DistributionStackBar ───────────────────────────────── */

const VERDICT_ORDER: DeltaEfficiency[] = [
  'no_reference',
  'poor',
  'fair',
  'good',
  'excellent',
];

function DistributionStackBar({
  counts,
  total,
}: {
  counts: Record<DeltaEfficiency, number>;
  total: number;
}) {
  const colors = useColors();
  if (total === 0) return null;

  const segmentColor = (e: DeltaEfficiency): string => {
    switch (e) {
      case 'excellent': return colors.ACCENT_GREEN;
      case 'good':      return colors.ACCENT_BLUE;
      case 'fair':      return colors.ACCENT_AMBER;
      case 'poor':      return colors.ACCENT_RED;
      case 'no_reference': return colors.ACCENT_PURPLE;
    }
  };

  return (
    <div>
      <div
        style={{
          display: 'flex',
          width: '100%',
          height: 14,
          borderRadius: 4,
          overflow: 'hidden',
          background: colors.BG_ELEVATED,
          border: `1px solid ${colors.BORDER}`,
        }}
        role="img"
        aria-label={
          `Verdict distribution: ` +
          VERDICT_ORDER
            .filter((e) => counts[e] > 0)
            .map((e) => `${counts[e]} ${efficiencyLabel(e).toLowerCase()}`)
            .join(', ')
        }
      >
        {VERDICT_ORDER.map((e) => {
          const n = counts[e];
          if (n === 0) return null;
          const pct = (n / total) * 100;
          return (
            <HoverHint
              key={e}
              hint={`${n} ${efficiencyLabel(e).toLowerCase()} (${pct.toFixed(1)}%)`}
              triggerStyle={{ width: `${pct}%`, height: '100%' }}
            >
              <div
                style={{
                  width: '100%',
                  height: '100%',
                  background: segmentColor(e),
                  transition: 'width 0.2s ease',
                }}
              />
            </HoverHint>
          );
        })}
      </div>
      {/* Legend — only show verdicts that have entries, in the same
          worst→best order as the bar. */}
      <div
        style={{
          display: 'flex',
          flexWrap: 'wrap',
          gap: 16,
          marginTop: 8,
          fontSize: 11,
          color: colors.TEXT_SECONDARY,
        }}
      >
        {VERDICT_ORDER.filter((e) => counts[e] > 0).map((e) => (
          <span key={e} style={{ display: 'inline-flex', alignItems: 'center', gap: 6 }}>
            <span
              style={{
                width: 10,
                height: 10,
                borderRadius: 2,
                background: segmentColor(e),
                flexShrink: 0,
              }}
            />
            <b style={{ color: colors.TEXT_PRIMARY }}>{counts[e]}</b>
            <span>{efficiencyLabel(e).toLowerCase()}</span>
          </span>
        ))}
      </div>
    </div>
  );
}

/* ── Band 3: RankedRatioRows ────────────────────────────────────── */

function RankedRatioRows({
  reports,
  verifyState,
  onVerify,
}: {
  reports: DeltaspaceEfficiencyReport[];
  verifyState: Map<string, VerifyState>;
  onVerify: (bucket: string, prefix: string) => void;
}) {
  const colors = useColors();
  if (reports.length === 0) {
    return (
      <Alert
        type="info"
        showIcon
        message="No prefixes match the current filter."
      />
    );
  }
  return (
    <div
      style={{
        border: `1px solid ${colors.BORDER}`,
        borderRadius: 8,
        overflow: 'hidden',
        background: colors.BG_CARD,
      }}
    >
      {/* Header. Column shape:
            [prefix (n deltas)] [reference fit bar] [reference] [stored] [actions]
       */}
      <div
        style={{
          display: 'grid',
          gridTemplateColumns: '1fr 260px 80px 130px 48px',
          gap: 12,
          padding: '10px 14px',
          borderBottom: `1px solid ${colors.BORDER}`,
          fontSize: 11,
          fontWeight: 700,
          letterSpacing: 0.5,
          textTransform: 'uppercase',
          color: colors.TEXT_MUTED,
          background: colors.BG_ELEVATED,
        }}
      >
        <HoverHint hint="Prefix in the bucket. The number in parentheses is the count of delta files measured (sample size — bigger = more reliable verdict).">
          <span style={{ borderBottom: `1px dotted ${colors.TEXT_MUTED}`, cursor: 'help' }}>
            Prefix (n)
          </span>
        </HoverHint>
        <HoverHint hint={<RatioHeaderHint />} maxWidth={320}>
          <span style={{ borderBottom: `1px dotted ${colors.TEXT_MUTED}`, cursor: 'help' }}>
            Reference fit
          </span>
        </HoverHint>
        <div style={{ textAlign: 'right' }}>Reference</div>
        <HoverHint hint="Bytes the deltas occupy on disk (reference excluded). Click ▶ Verify on a row to also show the originals' total and the % saved.">
          <span style={{ borderBottom: `1px dotted ${colors.TEXT_MUTED}`, cursor: 'help', display: 'inline-block', width: '100%', textAlign: 'right' }}>
            Stored / original
          </span>
        </HoverHint>
        <div />
      </div>
      {reports.map((r) => {
        const key = `${r.bucket}/${r.prefix}`;
        return (
          <RatioRow
            key={key}
            r={r}
            verify={verifyState.get(key)}
            onVerify={() => onVerify(r.bucket, r.prefix)}
          />
        );
      })}
    </div>
  );
}

/**
 * The headline metric is `median(delta_size) / reference_size`. It
 * tells the operator how big the typical delta is relative to the
 * baseline. The axis maps that ratio to a fixed [0×, 2×] range:
 *
 *   0×    — typical delta is empty (impossible in practice but the
 *           asymptote we'd love to approach).
 *   0.05× — Excellent threshold (delta ≤ 5% of reference).
 *   0.20× — Good ceiling.
 *   0.50× — Poor threshold.
 *   1.00× — the "cliff": delta is the same size as the reference.
 *           Right of this line the reference is actively unhelpful.
 *   2.00× — clip ceiling. Ratios > 2 (the screenshot's `1.14.0`
 *           lands around 60×) clamp the marker to the right edge;
 *           the numeric label still shows the true value.
 *
 * IMPORTANT: this is NOT a savings metric. It compares delta size to
 * reference size, not to the original file. Showing it as "% saved"
 * (which an earlier iteration did) implies the prefix is storing X%
 * more than originals, which the scanner has no way to determine
 * (originals aren't fetched in the HEAD-free fast path).
 */
const RATIO_AXIS_MIN = 0;
const RATIO_AXIS_MAX = 2;
const RATIO_CLIFF = 1.0;
const RATIO_POOR = 0.5;
const RATIO_EXCELLENT = 0.05;

function RatioRow({
  r,
  verify,
  onVerify,
}: {
  r: DeltaspaceEfficiencyReport;
  verify?: VerifyState;
  onVerify: () => void;
}) {
  const colors = useColors();
  const ratio = r.ratio_median;
  const ratioLabel = ratio == null ? '—' : formatDeltaToReference(ratio);
  const barColor = ratioBarColor(r.efficiency, colors);
  const verified = verify?.status === 'ok' ? verify.result : null;

  // The verified "true" ratio = total_stored / total_original (NOT
  // the median of per-file ratios — we want the bucket-level number
  // a CFO would understand). Render a second smaller diamond on the
  // SAME axis when we have it so the operator sees proxy-vs-truth
  // side by side. If true ratio > axis max (a prefix where DG truly
  // lost), we clamp at the right edge — same convention as the
  // primary marker.
  const verifiedRatio =
    verified && verified.total_original_bytes > 0
      ? verified.total_stored_bytes / verified.total_original_bytes
      : null;

  return (
    <div
      style={{
        display: 'grid',
        // 5 columns now (was 6) — Deltas merged into Prefix as "(n)".
        gridTemplateColumns: '1fr 260px 80px 130px 48px',
        gap: 12,
        alignItems: 'center',
        // Extra bottom padding so the axis labels (5% / 50% / 100%)
        // don't collide with the next row, and so the Stored block
        // (when verified) doesn't crowd the next row's marker.
        padding: '10px 14px 16px',
        borderBottom: `1px solid ${colors.BORDER}`,
        fontSize: 13,
      }}
    >
      {/* Prefix with sample-size suffix in muted grey. Operators
          want to scan prefix-by-prefix; the deltas count is context,
          not headline, so it rides as `(n)` rather than a column. */}
      <div
        style={{
          fontFamily: 'var(--font-mono)',
          color: colors.TEXT_PRIMARY,
          overflow: 'hidden',
          textOverflow: 'ellipsis',
          whiteSpace: 'nowrap',
        }}
        title={`s3://${r.bucket}/${r.prefix}/ — ${r.deltas.toLocaleString()} deltas`}
      >
        {r.prefix || '(root)'}{' '}
        <span style={{ color: colors.TEXT_MUTED, fontSize: 11 }}>
          ({r.deltas.toLocaleString()})
        </span>
      </div>

      {/* Reference-fit axis. Fixed [0×, 2×] scale with threshold
          lines at Excellent (5%), Poor (50%) and the "cliff" at
          100% (delta is the same size as the reference, right of
          which the reference is actively unhelpful). The numeric
          ratio is no longer rendered as text — the diamond's
          position on the axis is the answer, and the axis labels
          (5% / 50% / 100%) anchor it. Exact value is in the bar's
          hover. */}
      <RatioAxis
        ratio={ratio}
        ratioLabel={ratioLabel}
        color={barColor}
        verifiedRatio={verifiedRatio}
      />

      {/* Reference size — context for "is the waste real bytes or
          just a ratio in a tiny prefix" */}
      <div
        style={{
          fontFamily: 'var(--font-mono)',
          fontSize: 12,
          textAlign: 'right',
          color: colors.TEXT_SECONDARY,
        }}
      >
        {r.reference_bytes == null ? '—' : formatBytes(r.reference_bytes)}
      </div>

      {/* Stored / original. Without verification: just stored bytes.
          With verification: two lines, "Stored: X" + "Orig: Y
          (−Z% saved)" — answers the operator's "is DG actually
          saving bytes" question in one glance. */}
      <StoredCell r={r} verify={verify} verified={verified} />

      {/* Action affordances — explanation hint, Verify (opt-in HEAD
          deep-dive that returns true savings), copy URI. */}
      <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'flex-end', gap: 2 }}>
        <HoverHint hint={r.explanation}>
          <span
            style={{
              fontSize: 14,
              color:
                r.efficiency === 'poor' || r.efficiency === 'no_reference'
                  ? colors.ACCENT_RED
                  : colors.TEXT_MUTED,
            }}
            aria-label={`${efficiencyLabel(r.efficiency)} — ${r.explanation}`}
          >
            {r.efficiency === 'poor' || r.efficiency === 'no_reference' ? '⚠' : 'ⓘ'}
          </span>
        </HoverHint>
        <HoverHint
          hint={
            verified
              ? 'Re-verify (re-fetch originals)'
              : `Verify true savings (fires ${r.deltas.toLocaleString()} HEAD calls — should land in ~1s)`
          }
        >
          <Button
            size="small"
            type="text"
            loading={verify?.status === 'loading'}
            onClick={onVerify}
            style={{
              padding: 0,
              width: 22,
              height: 22,
              minWidth: 0,
              fontSize: 11,
              color: verified ? colors.ACCENT_GREEN : colors.TEXT_MUTED,
            }}
            aria-label={`Verify true savings for ${r.prefix}`}
          >
            ▶
          </Button>
        </HoverHint>
        <HoverHint hint="Copy s3:// URI">
          <Button
            size="small"
            type="text"
            icon={<CopyOutlined style={{ fontSize: 11 }} />}
            onClick={() => copyToClipboard(`s3://${r.bucket}/${r.prefix}/`)}
            style={{ padding: 0, width: 20, height: 20, minWidth: 0 }}
          />
        </HoverHint>
      </div>
    </div>
  );
}

/**
 * The Stored column body. Pre-verification it's a single number
 * (stored bytes, red+bold when the prefix is Poor / NoRef because
 * those are the rows whose stored bytes are recoverable). Post-
 * verification it gains a second line showing the originals' total
 * and the % saved (green = DG helped, red = DG hurt).
 *
 * Loading + error states render small inline indicators so the
 * column doesn't jump as the row upgrades.
 */
function StoredCell({
  r,
  verify,
  verified,
}: {
  r: DeltaspaceEfficiencyReport;
  verify?: VerifyState;
  verified: VerifyDeltaEfficiencyResponse | null;
}) {
  const colors = useColors();
  const isPoor = r.efficiency === 'poor' || r.efficiency === 'no_reference';
  const storedColor = isPoor ? colors.ACCENT_RED : colors.TEXT_PRIMARY;
  const storedWeight = isPoor ? 600 : 400;

  return (
    <div
      style={{
        fontFamily: 'var(--font-mono)',
        fontSize: 12,
        display: 'flex',
        flexDirection: 'column',
        alignItems: 'flex-end',
        gap: 1,
      }}
    >
      <div style={{ color: storedColor, fontWeight: storedWeight }}>
        {formatBytes(r.total_delta_bytes)}
      </div>
      {verified && <VerifiedStoredVsOriginal verified={verified} />}
      {verify?.status === 'loading' && (
        <div style={{ fontSize: 10, color: colors.TEXT_MUTED }}>verifying…</div>
      )}
      {verify?.status === 'error' && (
        <HoverHint hint={verify.message}>
          <span style={{ fontSize: 10, color: colors.ACCENT_RED, cursor: 'help' }}>
            verify failed
          </span>
        </HoverHint>
      )}
    </div>
  );
}

/**
 * Two-line post-verification overlay: "Orig: <X>" on top, "Stored:
 * <Y> (−Z% saved)" below — the operator's actual budget question
 * answered in one glance.
 *
 * Green when DG is helping (positive savings), red when DG is
 * actively hurting (the negative-savings case). Replaces the older
 * floating chip and ditches the "+X MB saved" framing, since the
 * user wants original-then-compressed-with-percent format.
 */
function VerifiedStoredVsOriginal({ verified }: { verified: VerifyDeltaEfficiencyResponse }) {
  const colors = useColors();
  const positive = verified.true_savings_bytes >= 0;
  const savedColor = positive ? colors.ACCENT_GREEN : colors.ACCENT_RED;
  const savedPct =
    verified.compression_ratio == null
      ? null
      : verified.compression_ratio * 100;

  // Sign-corrected percent: positive savings render as "−X%" (we
  // shrunk by X%), negative savings render as "+X%" (we GREW by X%).
  let pctLabel = '';
  if (savedPct != null) {
    if (savedPct >= 0) pctLabel = `−${savedPct.toFixed(1)}%`;
    else pctLabel = `+${Math.abs(savedPct).toFixed(1)}%`;
  }

  return (
    <HoverHint
      hint={
        <>
          <div>
            <b>Verified from {verified.deltas.toLocaleString()} HEAD calls.</b>
          </div>
          <div style={{ marginTop: 4 }}>
            Originals total: <b>{formatBytes(verified.total_original_bytes)}</b>
          </div>
          <div>
            DG storage: <b>{formatBytes(verified.total_stored_bytes)}</b>
          </div>
          <div style={{ marginTop: 4, color: savedColor }}>
            {positive
              ? `DG is saving ${formatBytes(verified.true_savings_bytes)} on this prefix.`
              : `DG is costing ${formatBytes(-verified.true_savings_bytes)} extra on this prefix — you'd be better off without it.`}
          </div>
        </>
      }
    >
      <div
        style={{
          display: 'flex',
          flexDirection: 'column',
          alignItems: 'flex-end',
          marginTop: 2,
          padding: '2px 6px',
          borderRadius: 4,
          background: `${savedColor}10`,
          border: `1px solid ${savedColor}30`,
          cursor: 'help',
          lineHeight: 1.25,
        }}
      >
        <div style={{ fontSize: 10, color: colors.TEXT_MUTED }}>
          orig {formatBytes(verified.total_original_bytes)}
        </div>
        <div
          style={{
            fontSize: 11,
            color: savedColor,
            fontWeight: 700,
            letterSpacing: 0.2,
          }}
        >
          {pctLabel}
        </div>
      </div>
    </HoverHint>
  );
}

/**
 * Render `median(delta) / reference` on a fixed [0×, 2×] axis. The
 * eye reads three things at once:
 *
 *   - WHERE the marker sits horizontally → magnitude of delta vs
 *     baseline. Far left = tiny delta (great). Far right = delta as
 *     big or bigger than the baseline (the reference isn't helping).
 *   - WHICH thresholds the marker has crossed → verdict (the green
 *     wash sits left of the 5% Excellent line; the red wash sits
 *     right of the 100% cliff where the reference is actively
 *     unhelpful).
 *   - HOW the row compares to its neighbours → all rows share the
 *     same axis, so eyes flicking up/down see relative position
 *     without reading numbers.
 *
 * Values above 2× clamp to the right edge; the numeric label still
 * shows the true value so nothing is hidden (the migration bucket
 * has prefixes at 60×).
 */
function RatioAxis({
  ratio,
  ratioLabel,
  color,
  verifiedRatio,
}: {
  ratio: number | null;
  /**
   * Pre-formatted label for the proxy ratio (e.g. "99%" or "6,000%").
   * The bar itself doesn't render this text inline (the diamond +
   * threshold lines carry the visual), but the bar's hover surfaces
   * the exact value so the operator can still read it on demand.
   */
  ratioLabel: string;
  color: string;
  /**
   * Optional verified ratio = `total_stored / total_original`. Once
   * supplied, it REPLACES the proxy marker (one marker per row, not
   * two). Reason: the two ratios answer different questions —
   *
   *   - proxy:    median(delta_size)     / reference_size
   *   - verified: total(stored_size)     / total(original_size)
   *
   * Showing both on the same axis suggested they were directly
   * comparable, which made operators ask "why are these so distant?"
   * The honest single-marker UX: pre-verify the diamond shows the
   * proxy; post-verify it shows the truth. The proxy value stays
   * accessible via the bar's hover so nothing is hidden.
   */
  verifiedRatio?: number | null;
}) {
  const colors = useColors();
  // The marker tracks the best signal we have: verified if present,
  // proxy otherwise. The hover always carries both values when both
  // exist so the operator can see the proxy was off.
  const displayRatio = verifiedRatio ?? ratio;
  const displayPct =
    displayRatio == null
      ? null
      : clamp(
          ((displayRatio - RATIO_AXIS_MIN) / (RATIO_AXIS_MAX - RATIO_AXIS_MIN)) * 100,
          0,
          100,
        );
  const excellentPct =
    ((RATIO_EXCELLENT - RATIO_AXIS_MIN) / (RATIO_AXIS_MAX - RATIO_AXIS_MIN)) * 100;
  const poorPct =
    ((RATIO_POOR - RATIO_AXIS_MIN) / (RATIO_AXIS_MAX - RATIO_AXIS_MIN)) * 100;
  const cliffPct =
    ((RATIO_CLIFF - RATIO_AXIS_MIN) / (RATIO_AXIS_MAX - RATIO_AXIS_MIN)) * 100;
  const isVerified = verifiedRatio != null;

  return (
    <HoverHint
      hint={
        <div style={{ minWidth: 200 }}>
          {!isVerified && (
            <div>
              <b>Proxy:</b> typical delta is {ratioLabel} of the reference.
            </div>
          )}
          {isVerified && (
            <>
              <div>
                <b>Verified:</b> deltas total{' '}
                <b>{(verifiedRatio * 100).toFixed(1)}%</b> of originals.
              </div>
              <div style={{ marginTop: 4, color: colors.TEXT_MUTED, fontSize: 11 }}>
                Proxy (median delta ÷ reference): {ratioLabel}. The proxy
                and the verified number answer different questions and
                can diverge — verified is the one to act on.
              </div>
            </>
          )}
        </div>
      }
      triggerStyle={{ display: 'block', width: '100%' }}
    >
    <div style={{ position: 'relative', height: 22, width: '100%' }}>
      {/* Track. Tinted bands give "left = good, right = bad" before
          numbers are read:
            [0,  5%]  → green wash (Excellent zone)
            [5, 100%] → neutral
            [100%, 200%] → red wash (reference is unhelpful) */}
      <div
        style={{
          position: 'absolute',
          inset: 0,
          borderRadius: 3,
          overflow: 'hidden',
          background: colors.BG_ELEVATED,
          border: `1px solid ${colors.BORDER}`,
        }}
      >
        <div
          style={{
            position: 'absolute',
            top: 0,
            bottom: 0,
            left: 0,
            width: `${excellentPct}%`,
            background: `${colors.ACCENT_GREEN}18`,
          }}
        />
        <div
          style={{
            position: 'absolute',
            top: 0,
            bottom: 0,
            left: `${cliffPct}%`,
            right: 0,
            background: `${colors.ACCENT_RED}14`,
          }}
        />
      </div>

      {/* Threshold lines */}
      <AxisLine
        at={excellentPct}
        color={`${colors.ACCENT_GREEN}80`}
        label="5%"
        labelColor={colors.TEXT_FAINT}
      />
      <AxisLine
        at={poorPct}
        color={`${colors.TEXT_MUTED}60`}
        label="50%"
        labelColor={colors.TEXT_FAINT}
      />
      <AxisLine
        at={cliffPct}
        color={colors.ACCENT_RED}
        label="100%"
        labelColor={colors.TEXT_MUTED}
      />

      {/* Single marker. Shape encodes provenance:
            - Diamond = proxy (unverified)
            - Diamond with inner dot = verified
          Same colour either way; same axis position is just the
          most truthful value we have. The marker animates to the new
          position when verify lands, making "the marker moved" the
          visual story rather than "two markers, why distant?". */}
      {displayPct != null && (
        <div
          style={{
            position: 'absolute',
            left: `${displayPct}%`,
            top: '50%',
            width: 12,
            height: 12,
            transform: 'translate(-50%, -50%) rotate(45deg)',
            background: color,
            border: `1.5px solid ${colors.BG_CARD}`,
            borderRadius: 2,
            boxShadow: `0 0 0 1px ${color}80`,
            transition: 'left 0.4s ease',
          }}
          aria-hidden
        >
          {/* Verified state — small inner dot to differentiate at a
              glance. Counter-rotated so it stays visually circular
              inside the rotated diamond. */}
          {isVerified && (
            <div
              style={{
                position: 'absolute',
                top: '50%',
                left: '50%',
                width: 5,
                height: 5,
                transform: 'translate(-50%, -50%) rotate(-45deg)',
                background: colors.BG_CARD,
                borderRadius: '50%',
              }}
            />
          )}
        </div>
      )}
    </div>
    </HoverHint>
  );
}

function AxisLine({
  at,
  color,
  label,
  labelColor,
}: {
  at: number;
  color: string;
  label?: string;
  labelColor?: string;
}) {
  return (
    <>
      <div
        style={{
          position: 'absolute',
          top: -2,
          bottom: -2,
          left: `${at}%`,
          width: 1,
          background: color,
          pointerEvents: 'none',
        }}
        aria-hidden
      />
      {label && (
        <div
          style={{
            position: 'absolute',
            left: `${at}%`,
            bottom: -14,
            transform: 'translateX(-50%)',
            fontSize: 9,
            fontFamily: 'var(--font-mono)',
            color: labelColor,
            letterSpacing: 0.3,
            pointerEvents: 'none',
            whiteSpace: 'nowrap',
          }}
        >
          {label}
        </div>
      )}
    </>
  );
}

/**
 * Format `delta / reference` ratio as a percentage. Used in the
 * RatioAxis hover and the verified-savings tooltip.
 *
 *   0.003 →  "0.3%"
 *   0.34  →  "34%"
 *   0.997 →  "100%"
 *   1.34  →  "134%"
 *   60.0  →  "6,000%"
 *
 * Decimal precision shrinks as the magnitude grows (one decimal under
 * 10%, none beyond) so the eye doesn't have to parse useless digits
 * on extreme values. Stays in % throughout — operators asked for
 * consistent units rather than switching to × notation at the cliff.
 */
function formatDeltaToReference(ratio: number): string {
  const pct = ratio * 100;
  if (pct < 10) return `${pct.toFixed(1)}%`;
  return `${Math.round(pct).toLocaleString()}%`;
}

function ratioBarColor(
  e: DeltaEfficiency,
  colors: ReturnType<typeof useColors>,
): string {
  switch (e) {
    case 'excellent': return colors.ACCENT_GREEN;
    case 'good':      return colors.ACCENT_BLUE;
    case 'fair':      return colors.ACCENT_AMBER;
    case 'poor':      return colors.ACCENT_RED;
    case 'no_reference': return colors.ACCENT_PURPLE;
  }
}

/**
 * Structured mini-card explaining the `delta ÷ reference (median)`
 * column. Replaces a wall of prose with a formula row, three colored
 * scale chips, and a one-line escape hatch pointing to Verify.
 *
 * Reads in about 4 seconds. The previous all-prose version was
 * ~120 words and took ~25 seconds.
 */
function RatioHeaderHint() {
  const colors = useColors();
  const rows: Array<{
    range: string;
    label: string;
    color: string;
    sub: string;
  }> = [
    {
      range: '≤ 5%',
      label: 'Good fit',
      color: colors.ACCENT_GREEN,
      sub: 'Reference compresses well.',
    },
    {
      range: '~ 100%',
      label: 'Cliff',
      color: colors.ACCENT_AMBER,
      sub: 'Delta is the same size as the reference.',
    },
    {
      range: '> 100%',
      label: 'Unhelpful',
      color: colors.ACCENT_RED,
      sub: 'Reference is actively hurting compression.',
    },
  ];

  return (
    <div style={{ minWidth: 260 }}>
      {/* Formula */}
      <div
        style={{
          padding: '6px 8px',
          background: `${colors.ACCENT_BLUE}10`,
          border: `1px solid ${colors.ACCENT_BLUE}30`,
          borderRadius: 5,
          fontFamily: 'var(--font-mono)',
          fontSize: 11,
          color: colors.TEXT_PRIMARY,
          textAlign: 'center',
          marginBottom: 10,
        }}
      >
        median(delta) ÷ reference
      </div>

      {/* Scale chips */}
      <div style={{ display: 'flex', flexDirection: 'column', gap: 6 }}>
        {rows.map((row) => (
          <div
            key={row.range}
            style={{
              display: 'grid',
              gridTemplateColumns: '54px 1fr',
              gap: 8,
              alignItems: 'baseline',
            }}
          >
            <div
              style={{
                fontFamily: 'var(--font-mono)',
                fontSize: 11,
                fontWeight: 700,
                color: row.color,
                textAlign: 'right',
              }}
            >
              {row.range}
            </div>
            <div>
              <div style={{ fontSize: 12, fontWeight: 600, color: colors.TEXT_PRIMARY }}>
                {row.label}
              </div>
              <div style={{ fontSize: 11, color: colors.TEXT_SECONDARY, lineHeight: 1.35 }}>
                {row.sub}
              </div>
            </div>
          </div>
        ))}
      </div>

      {/* Footer note — escape hatch for the "is DG actually saving
          bytes?" question this metric can't answer alone. */}
      <div
        style={{
          marginTop: 10,
          paddingTop: 8,
          borderTop: `1px dashed ${colors.BORDER}`,
          fontSize: 11,
          color: colors.TEXT_MUTED,
          lineHeight: 1.4,
        }}
      >
        This is a triage proxy, not total savings. Click <b style={{ color: colors.TEXT_PRIMARY }}>▶ Verify</b> on a row for true bytes-saved.
      </div>
    </div>
  );
}

