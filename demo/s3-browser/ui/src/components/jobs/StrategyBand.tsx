/**
 * StrategyBand — the "what algorithm did the engine apply" strip under a run's
 * outcome meter. Each copied object took one of three paths (shipped as-is /
 * rebuilt / straight copy); this names the mix in plain language with the
 * technical term on hover. Absent when nothing was copied. Layout logic is the
 * pure `jobStrategyMix` (jobsView + its regression test); this only maps to DOM.
 */
import type { StrategyMix } from '../../jobsView';
import { useColors } from '../../ThemeContext';
import { formatBytes } from '../../utils';

/** Per-path accent: verbatim = the win (teal), rebuilt = the cost (amber). */
function glyphColor(
  key: 'verbatim' | 'reconstructed' | 'straight',
  c: ReturnType<typeof useColors>,
): string {
  if (key === 'verbatim') return c.ACCENT_BLUE;
  if (key === 'reconstructed') return c.ACCENT_AMBER;
  return c.TEXT_MUTED;
}

export default function StrategyBand({ mix }: { mix: StrategyMix | null }) {
  const c = useColors();
  if (!mix || mix.segments.length === 0) return null;
  return (
    <div
      style={{
        display: 'flex',
        flexWrap: 'wrap',
        alignItems: 'baseline',
        gap: '4px 10px',
        fontSize: 12,
        lineHeight: 1.4,
        color: c.TEXT_MUTED,
        fontVariantNumeric: 'tabular-nums',
      }}
    >
      {mix.segments.map((s, i) => (
        <span key={s.key} style={{ display: 'inline-flex', alignItems: 'baseline', gap: 4 }}>
          {i > 0 && (
            <span aria-hidden="true" style={{ color: c.BORDER, marginRight: 6 }}>
              ·
            </span>
          )}
          <span aria-hidden="true" style={{ color: glyphColor(s.key, c) }}>
            {s.glyph}
          </span>
          <span title={s.hint} style={{ cursor: 'help' }}>
            <strong style={{ color: c.TEXT_PRIMARY, fontWeight: 600 }}>
              {s.count.toLocaleString()}
            </strong>{' '}
            {s.label}
          </span>
        </span>
      ))}
      {mix.bytesEgressSaved > 0 && (
        <span
          title="Bytes that never crossed the wire because the delta shipped as-is instead of the full object."
          style={{ cursor: 'help', color: c.ACCENT_GREEN }}
        >
          saved {formatBytes(mix.bytesEgressSaved)}
        </span>
      )}
    </div>
  );
}
