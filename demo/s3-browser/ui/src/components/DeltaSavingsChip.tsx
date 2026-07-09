import { useColors } from '../ThemeContext';
import { formatBytes } from '../utils';
import { summarizeScopeSavings } from '../savings';
import { useTweenedCount } from '../hooks/useTweenedCount';
import type { DeltaSummary } from '../deltaSummary';

interface Props {
  summary: DeltaSummary | null;
}

/**
 * A tiny pill rendered in the TopBar that surfaces the delta-
 * compression gains for the current prefix view. Auto-hides when no
 * deltas are present so the breadcrumb area stays clean on unmanaged
 * buckets.
 */
export default function DeltaSavingsChip({ summary }: Props) {
  const { STORAGE_TYPE_COLORS, TEXT_MUTED } = useColors();
  const palette = STORAGE_TYPE_COLORS['delta'];

  // Smooth count-up so the savings tick into place rather than snap.
  const target =
    summary != null ? Math.max(0, summary.originalBytes - summary.storedBytes) : 0;
  const animatedSaved = useTweenedCount(target);

  // Hide cases:
  //   - no summary at all
  //   - no deltas in scope (chip is "savings"-flavoured, not "stats")
  //   - server returned null savings_pct (empty prefix)
  if (!summary) return null;
  if (summary.deltaCount === 0) return null;
  if (summary.savingsPct == null) return null;

  // Route through the canonical scope-savings helper so the cap stays
  // in lockstep with every other surface. The server has already done
  // the math; we summarize again here only to share the same display
  // rules (integer floor + 99 ceiling for the scope view).
  const view = summarizeScopeSavings(summary.originalBytes, summary.storedBytes);
  const pct = view.pct;
  const savedLabel = formatBytes(animatedSaved);

  const refDetail = summary.referenceBytes > 0
    ? ` (incl. ${formatBytes(summary.referenceBytes)} reference)`
    : '';
  const tooltip = summary.loading
    ? `Scanning prefix…`
    : `${summary.deltaCount} delta${summary.deltaCount === 1 ? '' : 's'} of ${summary.totalCount} object${summary.totalCount === 1 ? '' : 's'} · ` +
      `${formatBytes(summary.originalBytes)} → ${formatBytes(summary.storedBytes)}${refDetail}` +
      (summary.truncated ? ' · scope truncated (>100k objects)' : '');

  return (
    <div
      role="status"
      aria-label={tooltip}
      title={tooltip}
      style={{
        display: 'inline-flex',
        alignItems: 'center',
        gap: 6,
        height: 22,
        padding: '0 8px',
        marginLeft: 12,
        borderRadius: 11,
        background: palette.bg,
        border: `1px solid ${palette.border}`,
        color: palette.text,
        fontFamily: 'var(--font-mono)',
        fontSize: 11,
        fontWeight: 600,
        lineHeight: 1,
        whiteSpace: 'nowrap',
        flexShrink: 0,
        opacity: summary.loading ? 0.6 : 1,
        transition: 'opacity 0.2s ease',
        animation: 'dgChipFadeIn 0.32s ease-out',
      }}
    >
      <svg
        width="11"
        height="11"
        viewBox="0 0 16 16"
        fill="none"
        aria-hidden="true"
        style={{ flexShrink: 0 }}
      >
        <path
          d="M8 2L13 8H10V13H6V8H3L8 2Z"
          fill={palette.text}
          transform="rotate(180 8 8)"
        />
      </svg>
      <span>
        {pct}% smaller{summary.truncated ? '+' : ''}
      </span>
      <span style={{ color: TEXT_MUTED, fontWeight: 400 }}>·</span>
      <span>{savedLabel} saved</span>
      {summary.loading && (
        <span style={{ color: TEXT_MUTED, fontWeight: 400, marginLeft: 2 }}>…</span>
      )}
      <style>{`
        @keyframes dgChipFadeIn {
          from { opacity: 0; transform: translateY(-2px); }
          to   { opacity: ${summary.loading ? 0.6 : 1}; transform: translateY(0); }
        }
      `}</style>
    </div>
  );
}
