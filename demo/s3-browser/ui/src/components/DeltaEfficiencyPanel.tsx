/**
 * DeltaEfficiencyPanel — v0.9.18 diagnostics surface.
 *
 * Backs onto `GET /_/api/admin/diagnostics/delta-efficiency` (see
 * `src/api/admin/delta_efficiency.rs`). For each bucket the operator
 * picks, scans every deltaspace and shows a per-prefix verdict so they
 * can spot prefixes whose reference baseline is wrong (the v0.9.17
 * 1.70.0-pre5 incident shape: a Kibana ZIP picked as the reference for
 * a prefix full of unrelated ES plugin ZIPs, producing 22 GB of
 * effectively-uncompressed deltas).
 *
 * Read-only. No mutation. Operator action is "re-upload the prefix"
 * which is offered as a copyable command rather than performed here —
 * that's a destructive operation we don't want one-click.
 */
import { useEffect, useMemo, useState } from 'react';
import { Typography, Button, Tag, Alert, Space, Select, InputNumber, Tooltip, Spin } from 'antd';
import { ReloadOutlined, ThunderboltOutlined, CopyOutlined } from '@ant-design/icons';
import { useColors } from '../ThemeContext';
import {
  fetchDeltaEfficiency,
  triggerDeltaEfficiencyScan,
  getBucketOrigins,
  type DeltaEfficiencyResponse,
  type DeltaspaceEfficiencyReport,
  type DeltaEfficiency,
} from '../adminApi';

const { Text, Paragraph } = Typography;

interface Props {
  onSessionExpired?: () => void;
}

/**
 * Map verdict → AntD Tag colour. Choices mirror the `actionColour`
 * helper in AuditLogPanel for visual consistency: red = bad,
 * volcano/orange = warn, green = good.
 */
function efficiencyColor(e: DeltaEfficiency): string {
  switch (e) {
    case 'excellent': return 'green';
    case 'good': return 'cyan';
    case 'fair': return 'gold';
    case 'poor': return 'red';
    case 'no_reference': return 'magenta';
  }
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

function fmtBytes(n: number | null | undefined): string {
  if (n == null) return '—';
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  if (n < 1024 * 1024 * 1024) return `${(n / 1024 / 1024).toFixed(2)} MB`;
  return `${(n / 1024 / 1024 / 1024).toFixed(2)} GB`;
}

function copyToClipboard(text: string) {
  if (navigator.clipboard && window.isSecureContext) {
    navigator.clipboard.writeText(text);
  }
}

export default function DeltaEfficiencyPanel({ onSessionExpired }: Props) {
  const colors = useColors();
  const [buckets, setBuckets] = useState<string[]>([]);
  const [bucket, setBucket] = useState<string | undefined>(undefined);
  const [minDeltas, setMinDeltas] = useState<number>(3);
  const [response, setResponse] = useState<DeltaEfficiencyResponse | null>(null);
  const [loading, setLoading] = useState(false);
  const [scanning, setScanning] = useState(false);
  const [error, setError] = useState<string | null>(null);

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
   * On 202, poll the GET endpoint every 2 s until we get a 200,
   * with a 60 s ceiling so a stuck scan doesn't spin forever.
   */
  const fetchOrPoll = async (
    forceRescan: boolean,
  ): Promise<DeltaEfficiencyResponse | null> => {
    if (!bucket) return null;
    if (forceRescan) {
      // Force a re-scan, then poll.
      await triggerDeltaEfficiencyScan(bucket, minDeltas);
    }
    const startedAt = Date.now();
    const deadline = startedAt + 60_000;
    // First call: if fresh-cached we'll get 200 immediately.
    // If not, the server enqueues a scan and we get 202.
    while (Date.now() < deadline) {
      const r = await fetchDeltaEfficiency(bucket, minDeltas);
      if ('scanning' in r) {
        // Server is working on it. Wait briefly, then re-fetch.
        setScanning(true);
        await new Promise(resolve => setTimeout(resolve, 2000));
        continue;
      }
      setScanning(false);
      return r;
    }
    throw new Error('Scan timed out after 60 s — try again or check server logs');
  };

  const runScan = async (forceRescan = false) => {
    if (!bucket) return;
    setLoading(true);
    setError(null);
    if (forceRescan) setResponse(null);
    try {
      const r = await fetchOrPoll(forceRescan);
      if (r) setResponse(r);
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      if (/401|session/i.test(msg)) onSessionExpired?.();
      setError(`Scan failed: ${msg}`);
    } finally {
      setLoading(false);
      setScanning(false);
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
        Scan a bucket and surface deltaspaces whose reference baseline is
        a poor fit for its sibling files. A wrong reference produces deltas
        nearly the size of the originals, wasting storage. Read-only
        diagnostic — you decide whether to re-upload.
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
        <Tooltip title="Skip prefixes with fewer than this many deltas. Smaller deltaspaces don't have enough signal to draw a verdict from.">
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
        </Tooltip>
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
          <Tooltip title="Force a fresh scan, ignoring the 5-min cache.">
            <Button
              icon={<ReloadOutlined />}
              onClick={() => runScan(true)}
              loading={loading}
            >
              Re-scan
            </Button>
          </Tooltip>
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
          <Space wrap style={{ marginBottom: 12 }}>
            <Text>
              Scanned <b>{response.scanned_deltaspaces}</b> deltaspace(s);
              reporting <b>{response.reported_deltaspaces}</b> with ≥ {response.min_deltas} deltas.
            </Text>
            {response.cached && (
              <Tooltip title={`Scan completed at ${response.computed_at}. The cache is good for 5 min — click Re-scan to force a fresh run.`}>
                <Tag color="blue">cached</Tag>
              </Tooltip>
            )}
            <Tag color="red">{summary.counts.poor} poor</Tag>
            <Tag color="magenta">{summary.counts.no_reference} no-ref</Tag>
            <Tag color="gold">{summary.counts.fair} fair</Tag>
            <Tag color="cyan">{summary.counts.good} good</Tag>
            <Tag color="green">{summary.counts.excellent} excellent</Tag>
            {summary.totalWasted > 0 && (
              <Text type="danger">
                ≈ {fmtBytes(summary.totalWasted)} stored as bad-reference deltas
              </Text>
            )}
          </Space>

          {response.reports.length === 0 ? (
            <Alert
              type="success"
              message="No deltaspaces met the reporting threshold"
              description={`No prefix in '${response.bucket}' has at least ${response.min_deltas} deltas. Try lowering 'min deltas' or pick another bucket.`}
              showIcon
            />
          ) : (
            <ReportsTable reports={response.reports} />
          )}
        </>
      )}
    </div>
  );
}

function ReportsTable({ reports }: { reports: DeltaspaceEfficiencyReport[] }) {
  const colors = useColors();
  return (
    <div style={{ overflowX: 'auto' }}>
      <table
        style={{
          width: '100%',
          borderCollapse: 'collapse',
          fontSize: 13,
          background: colors.BG_CARD,
        }}
      >
        <thead>
          <tr style={{ background: colors.BG_ELEVATED, color: colors.TEXT_PRIMARY }}>
            <th style={cellStyle}>Health</th>
            <th style={cellStyle}>Prefix</th>
            <th style={cellStyle}># deltas</th>
            <th style={cellStyle}>Reference</th>
            <th style={cellStyle}>Median delta</th>
            <th style={cellStyle}>Max delta</th>
            <th style={cellStyle}>Total stored</th>
            <th style={cellStyle}>Original</th>
            <th style={cellStyle}>Saved</th>
            <th style={cellStyle}>Why / suggestion</th>
          </tr>
        </thead>
        <tbody>
          {reports.map(r => (
            <tr key={`${r.bucket}/${r.prefix}`} style={{ borderTop: `1px solid ${colors.BORDER}` }}>
              <td style={cellStyle}>
                <Tag color={efficiencyColor(r.efficiency)}>{efficiencyLabel(r.efficiency)}</Tag>
              </td>
              <td style={{ ...cellStyle, fontFamily: 'monospace' }}>
                <span title={`s3://${r.bucket}/${r.prefix}`}>{r.prefix}</span>
                <Tooltip title="Copy s3:// URI">
                  <Button
                    size="small"
                    type="text"
                    icon={<CopyOutlined />}
                    onClick={() => copyToClipboard(`s3://${r.bucket}/${r.prefix}/`)}
                    style={{ marginLeft: 4 }}
                  />
                </Tooltip>
              </td>
              <td style={cellStyle}>{r.deltas}</td>
              <td style={cellStyle}>{fmtBytes(r.reference_bytes)}</td>
              <td style={cellStyle}>{fmtBytes(r.median_delta_bytes)}</td>
              <td style={cellStyle}>{fmtBytes(r.max_delta_bytes)}</td>
              <td style={cellStyle}>
                {fmtBytes(r.total_delta_bytes + (r.reference_bytes ?? 0))}
              </td>
              <td style={cellStyle}>{fmtBytes(r.total_original_bytes)}</td>
              <td style={cellStyle}>
                <Text type={r.savings_bytes < 0 ? 'danger' : undefined}>
                  {r.savings_bytes < 0 ? `-${fmtBytes(-r.savings_bytes)}` : fmtBytes(r.savings_bytes)}
                </Text>
              </td>
              <td style={{ ...cellStyle, color: colors.TEXT_SECONDARY, maxWidth: 360 }}>
                {r.explanation}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

const cellStyle: React.CSSProperties = {
  padding: '8px 10px',
  textAlign: 'left',
  verticalAlign: 'top',
  whiteSpace: 'nowrap',
};
