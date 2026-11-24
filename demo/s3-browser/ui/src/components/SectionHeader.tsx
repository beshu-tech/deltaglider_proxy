import { useColors } from '../ThemeContext';
import { tint } from '../themeVars';

/**
 * Card header: icon in its own column, title + description STACKED in a second
 * column so they share one left edge (the "invisible box") with no magic margin.
 * The icon is vertically centered against the title line. Title is the largest,
 * boldest thing in the card; description is clearly subordinate.
 *
 * Owns its own `marginBottom: 16` so the header→body gap is consistent across
 * every panel — call sites should NOT add their own `marginTop` to the wrapper
 * that immediately follows a SectionHeader.
 */
export default function SectionHeader({
  icon,
  title,
  description,
  tone = 'ACCENT_BLUE',
  children,
}: {
  icon: React.ReactNode;
  title: React.ReactNode;
  description?: string;
  /** The accent of the icon chip: a status card names its state with it. */
  tone?: 'ACCENT_BLUE' | 'ACCENT_GREEN' | 'ACCENT_AMBER' | 'ACCENT_RED';
  /** Body that shares the title's left edge. With it the header IS the card body: no bottom margin. */
  children?: React.ReactNode;
}) {
  const colors = useColors();
  const { TEXT_PRIMARY, TEXT_MUTED } = colors;
  const ACCENT_BLUE = colors[tone];
  return (
    <div style={{ display: 'flex', gap: 12, alignItems: 'flex-start', marginBottom: children ? 0 : 16 }}>
      <div
        style={{
          width: 32,
          height: 32,
          borderRadius: 8,
          background: `linear-gradient(135deg, ${tint(ACCENT_BLUE, 9)}, ${tint(ACCENT_BLUE, 3)})`,
          border: `1px solid ${tint(ACCENT_BLUE, 13)}`,
          display: 'flex',
          alignItems: 'center',
          justifyContent: 'center',
          flexShrink: 0,
          fontSize: 16,
          color: ACCENT_BLUE,
          // Nudge so the icon optically centers on the title's cap-height.
          marginTop: 1,
        }}
      >
        {icon}
      </div>
      <div style={{ minWidth: 0, flex: 1 }}>
        <div
          className="dg-display"
          style={{
            fontSize: 16,
            color: TEXT_PRIMARY,
            lineHeight: 1.25,
            letterSpacing: '-0.01em',
          }}
        >
          {title}
        </div>
        {description && (
          <div
            style={{
              fontFamily: 'var(--font-ui)',
              fontSize: 13,
              color: TEXT_MUTED,
              marginTop: 4,
              lineHeight: 1.45,
            }}
          >
            {description}
          </div>
        )}
        {children}
      </div>
    </div>
  );
}
