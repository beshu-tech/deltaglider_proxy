import { useColors } from '../../ThemeContext';

/** A run's outcome as a proportional bar of the work it ACTED ON
 *  (copied + errors): green = copied, red = errors. Scaling to work-done (not
 *  `scanned`) keeps the meaningful parts visible — on an incremental run almost
 *  everything is `skipped` (already in sync), which would otherwise make the
 *  bar ~100% blank. Skipped/scanned are surfaced on hover. Overlay = copied. */
export default function RunProgressBar({
  scanned,
  copied,
  errors,
  skipped,
}: {
  scanned: number;
  copied: number;
  errors: number;
  skipped: number;
}) {
  const c = useColors();
  const acted = copied + errors;
  const title =
    `${scanned.toLocaleString()} scanned · ` +
    `${copied.toLocaleString()} copied · ` +
    `${skipped.toLocaleString()} skipped (already in sync) · ` +
    `${errors.toLocaleString()} error${errors === 1 ? '' : 's'}`;

  // Nothing copied or errored yet (all scanned objects were skipped, or the run
  // just started): a flat in-sync bar with the copied count.
  const greenPct = acted === 0 ? 100 : (copied / acted) * 100;
  const redPct = acted === 0 ? 0 : (errors / acted) * 100;

  const label =
    copied.toLocaleString() +
    ' copied' +
    (errors > 0 ? ` · ${errors.toLocaleString()} error${errors === 1 ? '' : 's'}` : '');

  return (
    <div
      title={title}
      style={{
        position: 'relative',
        height: 18,
        minWidth: 90,
        borderRadius: 4,
        overflow: 'hidden',
        background: c.BORDER,
        display: 'flex',
        cursor: 'default',
      }}
    >
      {greenPct > 0 && (
        <div style={{ width: `${greenPct}%`, background: c.ACCENT_GREEN, height: '100%' }} />
      )}
      {redPct > 0 && (
        <div style={{ width: `${redPct}%`, background: c.ACCENT_RED, height: '100%' }} />
      )}
      <span
        style={{
          position: 'absolute',
          inset: 0,
          display: 'flex',
          alignItems: 'center',
          justifyContent: 'center',
          padding: '0 6px',
          fontSize: 11,
          fontWeight: 600,
          fontVariantNumeric: 'tabular-nums',
          color: acted === 0 ? c.TEXT_SECONDARY : '#0a0f1c',
          pointerEvents: 'none',
          // Clip to the pill — a squeezed cell must NOT bleed text into siblings.
          whiteSpace: 'nowrap',
          overflow: 'hidden',
          textOverflow: 'ellipsis',
        }}
      >
        {label}
      </span>
    </div>
  );
}
