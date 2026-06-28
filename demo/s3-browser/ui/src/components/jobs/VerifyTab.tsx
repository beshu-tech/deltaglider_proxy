/**
 * Verify tab — on-demand source-vs-dest parity audit for a replication rule.
 *
 * State machine: idle → loading → result (in-sync verdict | difference report)
 * → error. The GREEN "Verified in sync" verdict is the deliverable: a bespoke
 * framed panel (NOT a generic AntD success Alert) — a soft green halo around a
 * large ✓, the matched count as the visual anchor, an honest provenance footnote.
 *
 * Owns its own state via a useMutation (no poll, no auto-run on mount); it does
 * NOT touch the editable Definition form.
 */
import { Alert, Button, Table, Tag, Typography } from 'antd';
import {
  CaretRightOutlined,
  CheckOutlined,
  ReloadOutlined,
  SafetyCertificateOutlined,
  WarningOutlined,
} from '@ant-design/icons';
import type { FindingKind, ParityFinding, ParityOutcome, Verifier } from '../../adminApi';
import { conflictPolicyLabel, fixActionMeta, parityKindMeta, rerunVerdictMeta } from '../../jobsView';
import {
  useCancelVerify,
  useParityStatus,
  useRunReplicationNow,
  useStartVerify,
} from '../../queries/jobs';
import { useColors } from '../../ThemeContext';
import { timeAgo } from '../../utils';

const { Text } = Typography;

interface Props {
  ruleName: string;
}

/** Human label for the metadata source that produced a finding. */
function verifierLabel(v: Verifier | undefined): string | null {
  switch (v) {
    case 'sha256':
      return 'sha-256';
    case 'etag_size':
      return 'etag + size';
    case 'size_only':
      return 'size only';
    default:
      return null;
  }
}

/** Severity drives the halo + headline color: clean → green, mismatch → red, else amber. */
type Tone = 'green' | 'amber' | 'red';

function toneFor(o: ParityOutcome): Tone {
  if (o.in_sync) return 'green';
  if (o.checksum_mismatch > 0) return 'red';
  return 'amber';
}

export default function VerifyTab({ ruleName }: Props) {
  const c = useColors();
  // Server-side status: instant cached verdict on mount (survives navigation +
  // restart), polls while a scan runs. `start` POSTs to kick one off.
  const status = useParityStatus(ruleName);
  const start = useStartVerify(ruleName);
  const cancel = useCancelVerify(ruleName);
  const runNow = useRunReplicationNow();

  const s = status.data;
  const cancelling = s?.status === 'cancelling' || cancel.isPending;
  const running = s?.status === 'running' || cancelling || start.isPending;
  const outcome = s?.outcome;

  const run = () => start.mutate();
  const onCancel = () => cancel.mutate();
  // The one executable per-finding fix: run the rule, then re-verify.
  const onRunNow = () => runNow.mutate(ruleName, { onSuccess: () => start.mutate() });

  // ── first load (no server state yet) ───────────────────────────────────────
  if (status.isLoading) {
    return <LoadingBlock c={c} label="Loading verification status…" />;
  }

  // ── running with NO prior result → live progress + cancel ──────────────────
  if (running && !outcome) {
    return (
      <LoadingBlock
        c={c}
        label={cancelling ? 'Cancelling…' : 'Comparing source and destination…'}
        scanned={s?.progress_scanned}
        onCancel={cancelling ? undefined : onCancel}
      />
    );
  }

  // ── cancelled (and no prior result to show) ───────────────────────────────
  if (s?.status === 'cancelled' && !outcome) {
    return (
      <div style={{ padding: '8px 4px' }}>
        <Alert
          type="info"
          showIcon
          message="Verification cancelled"
          description="The parity audit was cancelled before it finished."
          style={{ borderRadius: 8, marginBottom: 16 }}
        />
        <Button
          type="primary"
          icon={<SafetyCertificateOutlined />}
          onClick={run}
          loading={start.isPending}
        >
          Run verification
        </Button>
      </div>
    );
  }

  // ── failed (and no prior result to show) ───────────────────────────────────
  if (s?.status === 'failed' && !outcome) {
    return (
      <div style={{ padding: '8px 4px' }}>
        <Alert
          type="error"
          showIcon
          message="Verification failed"
          description={s.error || 'The parity audit could not complete.'}
          style={{ borderRadius: 8, marginBottom: 16 }}
        />
        <Button icon={<ReloadOutlined />} onClick={run} loading={start.isPending}>
          Retry
        </Button>
      </div>
    );
  }

  // ── idle, never run ────────────────────────────────────────────────────────
  if (!outcome) {
    return (
      <div style={{ padding: '8px 4px' }}>
        <div style={{ display: 'flex', alignItems: 'center', gap: 10, marginBottom: 8 }}>
          <SafetyCertificateOutlined style={{ fontSize: 18, color: c.ACCENT_BLUE }} />
          <Text strong style={{ fontSize: 15 }}>
            Verify replication parity
          </Text>
        </div>
        <Text style={{ display: 'block', color: c.TEXT_SECONDARY, lineHeight: 1.5, marginBottom: 4 }}>
          Verify that every object in the source exists on the destination with a matching checksum.
        </Text>
        <Text type="secondary" style={{ display: 'block', fontSize: 12.5, marginBottom: 18 }}>
          Compares logical SHA-256 + size from metadata — no downloads. Runs in the background;
          the result is saved, so you can leave this page and come back.
        </Text>
        <Button
          type="primary"
          icon={<SafetyCertificateOutlined />}
          onClick={run}
          loading={start.isPending}
        >
          Run verification
        </Button>
      </div>
    );
  }

  // ── result (cached verdict; may be re-verifying in the background) ─────────
  return (
    <ParityResult
      outcome={outcome}
      reverifying={running}
      reverifyScanned={running ? s?.progress_scanned : undefined}
      onReverify={run}
      onCancelReverify={cancelling ? undefined : onCancel}
      onRunNow={onRunNow}
      runNowPending={runNow.isPending}
      c={c}
    />
  );
}

function LoadingBlock({
  c,
  label = 'Comparing source and destination…',
  scanned,
  onCancel,
}: {
  c: ReturnType<typeof useColors>;
  label?: string;
  scanned?: number;
  /** When provided, shows a Cancel button (a running audit). */
  onCancel?: () => void;
}) {
  return (
    <div
      style={{
        display: 'flex',
        flexDirection: 'column',
        alignItems: 'center',
        justifyContent: 'center',
        gap: 12,
        padding: 56,
      }}
    >
      <span className="dg-verify-spinner" aria-hidden />
      <Text type="secondary" style={{ fontSize: 12.5 }}>
        {label}
      </Text>
      {scanned != null && scanned > 0 && (
        <Text type="secondary" style={{ fontSize: 12, color: c.TEXT_MUTED, fontVariantNumeric: 'tabular-nums' }}>
          {scanned.toLocaleString()} objects scanned
        </Text>
      )}
      <Text type="secondary" style={{ fontSize: 11.5, color: c.TEXT_MUTED }}>
        Runs in the background — safe to navigate away.
      </Text>
      {onCancel && (
        <Button size="small" onClick={onCancel} style={{ marginTop: 4 }}>
          Cancel
        </Button>
      )}
      <style>{`
        .dg-verify-spinner {
          width: 28px; height: 28px; border-radius: 50%;
          border: 3px solid ${c.BORDER};
          border-top-color: ${c.ACCENT_BLUE};
          animation: dg-verify-spin 0.7s linear infinite;
        }
        @keyframes dg-verify-spin { to { transform: rotate(360deg); } }
      `}</style>
    </div>
  );
}

// ── The verdict panel ─────────────────────────────────────────────────────────

export function ParityResult({
  outcome,
  reverifying,
  reverifyScanned,
  onReverify,
  onCancelReverify,
  onRunNow,
  runNowPending,
  c,
}: {
  outcome: ParityOutcome;
  /** A background re-verify is running — show a subtle badge + spin the button. */
  reverifying?: boolean;
  /** Live objects-scanned of the in-flight re-verify (for the badge). */
  reverifyScanned?: number;
  onReverify: () => void;
  /** Cancel the in-flight re-verify. Omitted (or while cancelling) = no button. */
  onCancelReverify?: () => void;
  /** Executes the rule's run-now (the only executable fix). Omitted = disabled CTA. */
  onRunNow?: () => void;
  runNowPending?: boolean;
  c: ReturnType<typeof useColors>;
}) {
  const tone = toneFor(outcome);
  const inSync = outcome.in_sync;

  const haloColor =
    tone === 'green' ? c.ACCENT_GREEN : tone === 'red' ? c.ACCENT_RED : c.ACCENT_AMBER;
  const glow =
    tone === 'green'
      ? c.GLOW_GREEN
      : tone === 'red'
        ? 'rgba(251,113,133,0.30)'
        : 'rgba(251,191,36,0.30)';

  const totalDiffs =
    outcome.missing_on_dest +
    outcome.orphan_on_dest +
    outcome.checksum_mismatch +
    outcome.unverifiable;

  const scannedDate = new Date(outcome.scanned_at * 1000);
  const scannedAbsolute = scannedDate.toLocaleString();

  return (
    <div style={{ padding: '4px 2px' }}>
      {/* Framed verdict panel on elevated bg */}
      <div
        style={{
          background: c.BG_ELEVATED,
          border: `1px solid ${c.BORDER}`,
          borderRadius: 14,
          padding: '32px 24px 26px',
          textAlign: 'center',
          boxShadow: c.ELEV_SHADOW,
        }}
      >
        {/* Halo + glyph */}
        <div
          style={{
            width: 96,
            height: 96,
            margin: '0 auto 18px',
            borderRadius: '50%',
            display: 'flex',
            alignItems: 'center',
            justifyContent: 'center',
            background: `radial-gradient(circle at center, ${glow} 0%, transparent 70%)`,
          }}
        >
          <div
            style={{
              width: 72,
              height: 72,
              borderRadius: '50%',
              display: 'flex',
              alignItems: 'center',
              justifyContent: 'center',
              background: `${haloColor}1f`,
              border: `1.5px solid ${haloColor}66`,
            }}
          >
            {inSync ? (
              <CheckOutlined style={{ fontSize: 40, color: haloColor }} />
            ) : (
              <WarningOutlined style={{ fontSize: 38, color: haloColor }} />
            )}
          </div>
        </div>

        {/* Headline */}
        <div style={{ fontSize: 21, fontWeight: 700, color: c.TEXT_PRIMARY, marginBottom: 8 }}>
          {inSync
            ? outcome.truncated
              ? 'Verified in sync (scanned portion)'
              : 'Verified in sync'
            : `${totalDiffs.toLocaleString()} difference${totalDiffs === 1 ? '' : 's'} found`}
        </div>

        {/* Body line */}
        {inSync ? (
          <div style={{ fontSize: 14, color: c.TEXT_SECONDARY, lineHeight: 1.55, maxWidth: 440, margin: '0 auto' }}>
            All{' '}
            <span style={{ color: haloColor, fontWeight: 700 }}>
              {outcome.matched.toLocaleString()}
            </span>{' '}
            objects match by checksum. No missing files, no extras.
          </div>
        ) : (
          <div style={{ fontSize: 14, color: c.TEXT_SECONDARY, lineHeight: 1.55, maxWidth: 460, margin: '0 auto' }}>
            {outcome.matched.toLocaleString()} of {outcome.source_objects.toLocaleString()} source
            objects match. The destination is not an exact mirror.
          </div>
        )}

        {/* Rule-context line — WHY the verdicts read as they do. */}
        <RuleContextLine outcome={outcome} c={c} />

        {/* Difference chips (only when not in sync) */}
        {!inSync && (
          <div
            style={{
              display: 'flex',
              flexWrap: 'wrap',
              gap: 8,
              justifyContent: 'center',
              marginTop: 16,
            }}
          >
            {outcome.missing_on_dest > 0 && (
              <Chip color={c.ACCENT_AMBER} text={`${outcome.missing_on_dest.toLocaleString()} missing on destination`} />
            )}
            {outcome.orphan_on_dest > 0 && (
              <Chip color={c.ACCENT_BLUE} text={`${outcome.orphan_on_dest.toLocaleString()} extra on destination`} />
            )}
            {outcome.checksum_mismatch > 0 && (
              <Chip color={c.ACCENT_RED} text={`${outcome.checksum_mismatch.toLocaleString()} checksum mismatch`} />
            )}
            {outcome.unverifiable > 0 && (
              <Chip color={c.TEXT_MUTED} text={`${outcome.unverifiable.toLocaleString()} matched on size only`} />
            )}
          </div>
        )}

        {/* Honest, sample-scoped actionable summary (only when not in sync). */}
        {!inSync && <ActionableLine outcome={outcome} c={c} />}

        {/* Provenance footnote */}
        <div
          style={{ fontSize: 12, color: c.TEXT_MUTED, marginTop: 18 }}
          title={`Scanned at ${scannedAbsolute}`}
        >
          Checked {timeAgo(scannedDate)} · logical SHA-256 + size, from metadata
        </div>

        <div style={{ marginTop: 18, display: 'flex', gap: 8, justifyContent: 'center', alignItems: 'center' }}>
          <Button icon={<ReloadOutlined />} onClick={onReverify} loading={reverifying}>
            {reverifying ? 'Re-verifying…' : inSync ? 'Re-verify' : 'Verify again'}
          </Button>
          {reverifying && onCancelReverify && (
            <Button size="small" onClick={onCancelReverify}>
              Cancel
            </Button>
          )}
          {reverifying && reverifyScanned != null && reverifyScanned > 0 && (
            <Text type="secondary" style={{ fontSize: 12, color: c.TEXT_MUTED, fontVariantNumeric: 'tabular-nums' }}>
              {reverifyScanned.toLocaleString()} scanned
            </Text>
          )}
        </div>
      </div>

      {/* Truncation note */}
      {outcome.truncated && (
        <Alert
          type="info"
          showIcon
          style={{ borderRadius: 8, marginTop: 14 }}
          message="Scan capped"
          description={`The audit stopped after the first ${(
            outcome.source_objects + outcome.dest_objects
          ).toLocaleString()} objects. Counts and findings cover only the scanned portion.`}
        />
      )}

      {/* Unverifiable-only case: nothing is missing/extra/corrupt, but some
          objects could only be size-matched (foreign, no logical SHA-256). */}
      {!inSync &&
        outcome.unverifiable > 0 &&
        outcome.missing_on_dest === 0 &&
        outcome.orphan_on_dest === 0 &&
        outcome.checksum_mismatch === 0 && (
          <Alert
            type="info"
            showIcon
            style={{ borderRadius: 8, marginTop: 14 }}
            message="Some objects matched on size only"
            description="These objects weren't written through the proxy, so no logical SHA-256 is available — write them through the proxy for full checksum parity."
          />
        )}

      {/* Findings table */}
      {!inSync && (
        <FindingsTable
          outcome={outcome}
          c={c}
          onRunNow={onRunNow}
          runNowPending={runNowPending}
        />
      )}
    </div>
  );
}

/** "Conflict policy: <human> · Mirror delete: on/off" — sets up the verdicts. */
function RuleContextLine({
  outcome,
  c,
}: {
  outcome: ParityOutcome;
  c: ReturnType<typeof useColors>;
}) {
  return (
    <div style={{ fontSize: 12.5, color: c.TEXT_MUTED, marginTop: 12 }}>
      Conflict policy:{' '}
      <span style={{ color: c.TEXT_SECONDARY, fontWeight: 600 }}>
        {conflictPolicyLabel(outcome.conflict_policy)}
      </span>{' '}
      · Mirror delete:{' '}
      <span style={{ color: c.TEXT_SECONDARY, fontWeight: 600 }}>
        {outcome.replicate_deletes ? 'on' : 'off'}
      </span>
    </div>
  );
}

/**
 * Honest, sample-scoped one-liner derived from `outcome.actionable`:
 * "Re-run will fix N · M need attention · K failing". Each clause only renders
 * when its count is non-zero; the whole line is hidden when nothing is actionable.
 */
function ActionableLine({
  outcome,
  c,
}: {
  outcome: ParityOutcome;
  c: ReturnType<typeof useColors>;
}) {
  const a = outcome.actionable;
  const parts: string[] = [];
  if (a.rerun_fixes > 0) parts.push(`Re-run will fix ${a.rerun_fixes.toLocaleString()}`);
  if (a.rerun_conditional > 0)
    parts.push(`${a.rerun_conditional.toLocaleString()} depend on timestamps`);
  if (a.needs_manual > 0) parts.push(`${a.needs_manual.toLocaleString()} need attention`);
  if (a.copy_failing > 0) parts.push(`${a.copy_failing.toLocaleString()} failing`);
  if (parts.length === 0) return null;
  return (
    <div
      style={{ fontSize: 13, color: c.TEXT_SECONDARY, marginTop: 14, lineHeight: 1.5 }}
      title="Counts cover the sampled findings shown below, not the exact totals."
    >
      {parts.join(' · ')}
      <span style={{ color: c.TEXT_MUTED }}> (in the sampled findings)</span>
    </div>
  );
}

function Chip({ color, text }: { color: string; text: string }) {
  return (
    <span
      style={{
        display: 'inline-flex',
        alignItems: 'center',
        gap: 6,
        fontSize: 12.5,
        fontWeight: 600,
        padding: '3px 11px',
        borderRadius: 999,
        color,
        background: `${color}1a`,
        border: `1px solid ${color}40`,
      }}
    >
      <span style={{ width: 7, height: 7, borderRadius: '50%', background: color }} />
      {text}
    </span>
  );
}

function FindingsTable({
  outcome,
  c,
  onRunNow,
  runNowPending,
}: {
  outcome: ParityOutcome;
  c: ReturnType<typeof useColors>;
  onRunNow?: () => void;
  runNowPending?: boolean;
}) {
  // Concat the per-category samples (each capped at 100 server-side).
  const rows: ParityFinding[] = [
    ...outcome.missing_samples,
    ...outcome.orphan_samples,
    ...outcome.mismatch_samples,
  ];
  const shown = rows.length;
  const totalDiffsExclUnverifiable =
    outcome.missing_on_dest + outcome.orphan_on_dest + outcome.checksum_mismatch;
  const capped = shown < totalDiffsExclUnverifiable;

  return (
    <div style={{ marginTop: 18 }}>
      <Text strong style={{ display: 'block', fontSize: 13.5, marginBottom: 8 }}>
        Differences
      </Text>
      <Table<ParityFinding>
        dataSource={rows}
        rowKey={(f) => `${f.kind}:${f.key}`}
        size="small"
        pagination={false}
        locale={{ emptyText: 'No sampled differences' }}
        columns={[
          {
            title: 'Object',
            render: (_: unknown, f) => (
              <Text
                style={{
                  fontFamily: 'var(--font-mono)',
                  fontSize: 12,
                  display: 'inline-block',
                  maxWidth: 220,
                  overflow: 'hidden',
                  textOverflow: 'ellipsis',
                  whiteSpace: 'nowrap',
                  verticalAlign: 'bottom',
                }}
                title={f.key}
              >
                {f.key}
              </Text>
            ),
          },
          {
            title: 'Issue',
            width: 140,
            render: (_: unknown, f) => {
              const kind: FindingKind = f.kind;
              const meta = parityKindMeta(kind);
              return <Tag color={meta.color}>{meta.label}</Tag>;
            },
          },
          {
            title: 'Why',
            width: 250,
            render: (_: unknown, f) => <WhyCell finding={f} c={c} />,
          },
          {
            title: 'Fix',
            width: 230,
            render: (_: unknown, f) => (
              <FixCell finding={f} c={c} onRunNow={onRunNow} runNowPending={runNowPending} />
            ),
          },
        ]}
      />
      {capped && (
        <Text type="secondary" style={{ display: 'block', fontSize: 12, marginTop: 8, color: c.TEXT_MUTED }}>
          Showing first {shown.toLocaleString()} of {totalDiffsExclUnverifiable.toLocaleString()} differences
        </Text>
      )}
      <Text type="secondary" style={{ display: 'block', fontSize: 12, marginTop: 8, color: c.TEXT_MUTED }}>
        After fixing, run verification again to confirm.
      </Text>
    </div>
  );
}

/** Why = verdict chip (re-run will/won't help) + the backend's reason_detail. */
function WhyCell({ finding, c }: { finding: ParityFinding; c: ReturnType<typeof useColors> }) {
  const rem = finding.remediation;
  // Defensive: un-annotated finding (shouldn't happen) → fall back to old detail.
  if (!rem) {
    const vl = verifierLabel(finding.verifier);
    return (
      <Text type="secondary" style={{ fontSize: 12 }}>
        {finding.detail}
        {vl && <span style={{ color: c.TEXT_MUTED }}> · {vl}</span>}
      </Text>
    );
  }
  const v = rerunVerdictMeta(rem.rerun_helps);
  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 4 }}>
      <Tag color={v.color} style={{ marginInlineEnd: 0, whiteSpace: 'normal' }}>
        {v.label}
      </Tag>
      <Text type="secondary" style={{ fontSize: 12, lineHeight: 1.45 }}>
        {rem.reason_detail}
      </Text>
    </div>
  );
}

/**
 * Fix = a "Run now" button when re-run is the fix (the only executable action),
 * otherwise an instructional chip + muted one-line guidance (native `title`
 * carries the long form). Un-annotated findings render nothing here.
 */
function FixCell({
  finding,
  c,
  onRunNow,
  runNowPending,
}: {
  finding: ParityFinding;
  c: ReturnType<typeof useColors>;
  onRunNow?: () => void;
  runNowPending?: boolean;
}) {
  const rem = finding.remediation;
  if (!rem) return <Text type="secondary" style={{ fontSize: 12, color: c.TEXT_MUTED }}>—</Text>;
  const meta = fixActionMeta(rem.fix, rem.fix_detail);

  if (meta.runnable) {
    return (
      <div style={{ display: 'flex', flexDirection: 'column', gap: 4 }}>
        <Button
          size="small"
          type="primary"
          icon={<CaretRightOutlined />}
          loading={runNowPending}
          disabled={!onRunNow}
          onClick={onRunNow}
          title={onRunNow ? rem.fix_detail : 'Run-now is unavailable here'}
        >
          Run now
        </Button>
        <Text type="secondary" style={{ fontSize: 11.5, color: c.TEXT_MUTED, lineHeight: 1.4 }}>
          {rem.fix_detail}
        </Text>
      </div>
    );
  }

  // Guidance only — no write button. Chip + how-to (title carries fix_detail).
  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 4 }} title={rem.fix_detail}>
      <Tag style={{ marginInlineEnd: 0, whiteSpace: 'normal' }}>{meta.label}</Tag>
      <Text type="secondary" style={{ fontSize: 11.5, color: c.TEXT_MUTED, lineHeight: 1.4 }}>
        {meta.how ?? rem.fix_detail}
      </Text>
    </div>
  );
}
