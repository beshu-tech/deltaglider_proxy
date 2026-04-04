import { Button } from 'antd';
import { ArrowLeftOutlined, SunOutlined, MoonOutlined } from '@ant-design/icons';
import { useColors, useTheme } from '../ThemeContext';

interface Props {
  title: string;
  onBack: () => void;
  /** Optional right-side content (e.g. backup buttons) */
  extra?: React.ReactNode;
}

/** Shared header bar for full-screen views (Admin, Docs) */
export default function FullScreenHeader({ title, onBack, extra }: Props) {
  const colors = useColors();
  const { isDark, toggleTheme } = useTheme();
  return (
    <div style={{
      display: 'flex',
      alignItems: 'center',
      justifyContent: 'space-between',
      padding: '0 20px',
      height: 52,
      borderBottom: `1px solid ${colors.BORDER}`,
      background: colors.BG_CARD,
      flexShrink: 0,
    }}>
      {/* Left: back button */}
      <Button
        type="text"
        icon={<ArrowLeftOutlined />}
        onClick={onBack}
        style={{
          color: colors.TEXT_SECONDARY,
          fontWeight: 500,
          fontFamily: 'var(--font-ui)',
        }}
      >
        Browser
      </Button>

      {/* Center: branding + section title */}
      <div style={{
        display: 'flex',
        alignItems: 'baseline',
        gap: 10,
        userSelect: 'none',
      }}>
        <span style={{
          fontSize: 13,
          fontWeight: 700,
          letterSpacing: 3,
          color: colors.TEXT_MUTED,
          fontFamily: 'var(--font-ui)',
          textTransform: 'uppercase',
        }}>
          DeltaGlider
        </span>
        <span style={{
          width: 1,
          height: 14,
          background: colors.BORDER,
          display: 'inline-block',
          verticalAlign: 'middle',
          position: 'relative',
          top: 1,
        }} />
        <span style={{
          fontSize: 13,
          fontWeight: 600,
          letterSpacing: 2,
          color: colors.ACCENT_BLUE,
          fontFamily: 'var(--font-mono)',
          textTransform: 'uppercase',
        }}>
          {title}
        </span>
      </div>

      {/* Right: extra content + theme toggle */}
      <div style={{ minWidth: 100, display: 'flex', alignItems: 'center', justifyContent: 'flex-end', gap: 4 }}>
        {extra}
        <Button
          type="text"
          icon={isDark ? <MoonOutlined /> : <SunOutlined />}
          size="small"
          title={isDark ? 'Switch to light mode' : 'Switch to dark mode'}
          onClick={toggleTheme}
          style={{ color: colors.TEXT_MUTED, transition: 'color 0.15s' }}
        />
      </div>
    </div>
  );
}
