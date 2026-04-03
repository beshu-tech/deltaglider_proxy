import { Button } from 'antd';
import { ArrowLeftOutlined } from '@ant-design/icons';
import { useColors } from '../ThemeContext';

interface Props {
  title: string;
  onBack: () => void;
  /** Optional right-side content (e.g. backup buttons) */
  extra?: React.ReactNode;
}

/** Shared header bar for full-screen views (Admin, Docs) */
export default function FullScreenHeader({ title, onBack, extra }: Props) {
  const colors = useColors();
  return (
    <div style={{
      display: 'flex',
      alignItems: 'center',
      justifyContent: 'space-between',
      padding: '8px 16px',
      borderBottom: `1px solid ${colors.BORDER}`,
      background: colors.BG_CARD,
      flexShrink: 0,
    }}>
      <Button
        type="text"
        icon={<ArrowLeftOutlined />}
        onClick={onBack}
        style={{ color: colors.TEXT_SECONDARY, fontWeight: 500 }}
      >
        Back to Browser
      </Button>
      <span style={{
        fontSize: 15,
        fontFamily: 'var(--font-ui)',
        fontWeight: 700,
        letterSpacing: 1,
        textTransform: 'uppercase',
        color: colors.TEXT_MUTED,
      }}>
        {title}
      </span>
      {extra || <div style={{ width: 140 }} />}
    </div>
  );
}
