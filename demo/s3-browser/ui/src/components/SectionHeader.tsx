import { useColors } from '../ThemeContext';

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
}: {
  icon: React.ReactNode;
  title: React.ReactNode;
  description?: string;
}) {
  const { ACCENT_BLUE, TEXT_PRIMARY, TEXT_MUTED } = useColors();
  return (
    <div style={{ display: 'flex', gap: 12, alignItems: 'flex-start', marginBottom: 16 }}>
      <div
        style={{
          width: 32,
          height: 32,
          borderRadius: 8,
          background: `linear-gradient(135deg, ${ACCENT_BLUE}18, ${ACCENT_BLUE}08)`,
          border: `1px solid ${ACCENT_BLUE}22`,
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
          style={{
            fontFamily: 'var(--font-ui)',
            fontSize: 17,
            fontWeight: 700,
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
      </div>
    </div>
  );
}
