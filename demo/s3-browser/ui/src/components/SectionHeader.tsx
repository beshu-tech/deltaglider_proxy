import { useColors } from '../ThemeContext';
import { tint } from '../themeVars';

/**
 * Card header. ONE left edge for the whole card: the icon and the title share
 * the first line, and the description, like every field under the header,
 * starts at the card's own edge. (The description used to sit beside the icon,
 * which gave each card two left edges: 44 px in for the header text, 0 for the
 * fields.) Title is the largest, boldest thing in the card.
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
    <div style={{ marginBottom: children ? 0 : 16 }}>
      <div style={{ display: 'flex', gap: 12, alignItems: 'center' }}>
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
          }}
        >
          {icon}
        </div>
        <div
          className="dg-display"
          style={{
            minWidth: 0,
            flex: 1,
            fontSize: 16,
            color: TEXT_PRIMARY,
            lineHeight: 1.25,
            letterSpacing: '-0.01em',
          }}
        >
          {title}
        </div>
      </div>
      {description && (
        <div
          style={{
            fontFamily: 'var(--font-ui)',
            fontSize: 13,
            color: TEXT_MUTED,
            marginTop: 8,
            lineHeight: 1.45,
          }}
        >
          {description}
        </div>
      )}
      {children}
    </div>
  );
}
