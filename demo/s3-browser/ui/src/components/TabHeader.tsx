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
        textAlign: 'center',
        padding: '32px 24px 24px',
        borderBottom: `1px solid ${colors.BORDER}`,
        marginBottom: 0,
      }}
      role="heading"
      aria-level={2}
    >
      <div
        style={{
          display: 'inline-flex',
          alignItems: 'center',
          justifyContent: 'center',
          width: 40,
          height: 40,
          borderRadius: 10,
          background: `${colors.ACCENT_BLUE}12`,
          border: `1px solid ${colors.ACCENT_BLUE}25`,
          fontSize: 18,
          color: colors.ACCENT_BLUE,
          marginBottom: 12,
        }}
      >
        {icon}
      </div>
      <div
        style={{
          fontSize: 20,
          fontWeight: 700,
          color: colors.TEXT_PRIMARY,
          fontFamily: 'var(--font-ui)',
          letterSpacing: '-0.01em',
          lineHeight: 1.2,
        }}
      >
        {title}
      </div>
      <div
        style={{
          fontSize: 13,
          color: colors.TEXT_MUTED,
          fontFamily: 'var(--font-ui)',
          marginTop: 6,
          maxWidth: 480,
          marginLeft: 'auto',
          marginRight: 'auto',
          lineHeight: 1.5,
        }}
      >
        {description}
      </div>
    </div>
  );
}
