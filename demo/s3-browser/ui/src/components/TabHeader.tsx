import { useColors } from '../ThemeContext';

/**
 * Centered tab header with icon, title, and description.
 * Used at the top of every admin settings tab for consistent wayfinding.
 */

interface Props {
  icon: React.ReactNode;
  title: string;
  description: string;
}

export default function TabHeader({ icon, title, description }: Props) {
  const colors = useColors();

  return (
    <div
      style={{
        display: 'flex',
        alignItems: 'center',
        gap: 10,
        padding: '12px 20px',
        borderBottom: `1px solid ${colors.BORDER}`,
        marginBottom: 0,
      }}
      role="heading"
      aria-level={2}
    >
      <div
        aria-hidden="true"
        style={{
          display: 'flex',
          alignItems: 'center',
          justifyContent: 'center',
          width: 28,
          height: 28,
          borderRadius: 8,
          background: `${colors.ACCENT_BLUE}12`,
          border: `1px solid ${colors.ACCENT_BLUE}25`,
          fontSize: 14,
          color: colors.ACCENT_BLUE,
          flexShrink: 0,
        }}
      >
        {icon}
      </div>
      <div
        style={{
          minWidth: 0,
          flex: 1,
        }}
      >
        <div
          style={{
            fontSize: 16,
            fontWeight: 700,
            color: colors.TEXT_PRIMARY,
            fontFamily: 'var(--font-ui)',
            letterSpacing: '-0.01em',
            lineHeight: 1.15,
          }}
        >
          {title}
        </div>
        <div
          style={{
            fontSize: 12,
            color: colors.TEXT_MUTED,
            fontFamily: 'var(--font-ui)',
            marginTop: 2,
            maxWidth: 760,
            lineHeight: 1.35,
          }}
        >
          {description}
        </div>
      </div>
    </div>
  );
}
